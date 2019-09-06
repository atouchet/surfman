//! Wrapper for EGL contexts managed by ANGLE using Direct3D 11 as a backend on Windows.

use crate::{ContextAttributeFlags, ContextAttributes, Error, GLApi, GLFlavor, GLInfo};
use crate::{GLVersion, ReleaseContext};
use super::adapter::Adapter;
use super::device::Device;
use super::error::ToWindowingApiError;
use super::surface::{ColorSurface, Surface, SurfaceTexture};
use cgl::{CGLChoosePixelFormat, CGLContextObj, CGLCreateContext, CGLDescribePixelFormat};
use cgl::{CGLDestroyContext, CGLError, CGLGetCurrentContext, CGLGetPixelFormat};
use cgl::{CGLPixelFormatAttribute, CGLPixelFormatObj, CGLSetCurrentContext, kCGLPFAAlphaSize};
use cgl::{kCGLPFADepthSize, kCGLPFAStencilSize, kCGLPFAOpenGLProfile};
use core_foundation::base::TCFType;
use core_foundation::bundle::{CFBundleGetBundleWithIdentifier, CFBundleGetFunctionPointerForName};
use core_foundation::string::CFString;
use gl;
use gl::types::GLuint;
use std::mem;
use std::os::raw::c_void;
use std::ptr;
use std::str::FromStr;
use std::sync::Mutex;
use std::thread;

pub struct Context {
    pub(crate) egl_context: EGLContext,
    gl_info: GLInfo,
    color_surface: ColorSurface,
    releaser: Releaser,
}

pub type NativeContext = EGLContext;

type Releaser = Box<dyn ReleaseContext<Context = NativeContext>>;

impl Drop for Context {
    #[inline]
    fn drop(&mut self) {
        if !self.cgl_context.is_null() && !thread::panicking() {
            panic!("Contexts must be destroyed explicitly with `destroy_context`!")
        }
    }
}

lazy_static! {
    static ref CREATE_CONTEXT_MUTEX: Mutex<bool> = Mutex::new(false);
}

impl Device {
    /// Opens the device and context corresponding to the current EGL context.
    ///
    /// The `Releaser` callback will be called when the context is destroyed.
    ///
    /// This method is designed to allow `surfman` to deal with contexts created outside the
    /// library; for example, by Glutin. It's legal to use this method to wrap a context rendering
    /// to any target: either a window or a pbuffer. The target is opaque to `surfman`; the library
    /// will not modify or try to detect the render target. This means that any of the methods that
    /// query or replace the surface—e.g. `replace_context_color_surface`—will fail if called with
    /// a context object created via this method.
    pub unsafe fn from_current_context(releaser: Releaser) -> Result<(Device, Context), Error> {
        let mut previous_context_created = CREATE_CONTEXT_MUTEX.lock().unwrap();

        // Grab the current EGL display and EGL context.
        let egl_display = egl::GetCurrentDisplay();
        debug_assert_ne!(egl_display, egl::NO_DISPLAY);
        let egl_context = egl::GetCurrentContext();
        debug_assert_ne!(egl_context, egl::NO_CONTEXT);

        println!("Device::from_current_context() = {:x}", egl_context);

        // Fetch the EGL device.
        let mut egl_device = EGL_NO_DEVICE_EXT;
        let result = eglQueryDisplayAttribEXT(egl_display, EGL_DEVICE_EXT, &mut egl_device);
        assert_ne!(result, egl::FALSE);
        debug_assert_ne!(egl_device, EGL_NO_DEVICE_EXT);

        // Fetch the D3D11 device.
        let mut d3d11_device = ptr::null_mut();
        let result = eglQueryDeviceAttribEXT(egl_device,
                                             EGL_D3D11_DEVICE_ANGLE,
                                             &mut d3d11_device);
        assert_ne!(result, egl::FALSE);
        assert!(!d3d11_device.is_null());

        // Create the device wrapper.
        // FIXME(pcwalton): Using `D3D_DRIVER_TYPE_UNKNOWN` is unfortunate.
        let device = Device {
            egl_device,
            egl_display,
            surface_bindings: vec![],
            d3d11_device,
            d3d_driver_type: D3D_DRIVER_TYPE_UNKNOWN,
        };

        // Detect the GL version.
        let mut client_version = 0;
        let result = egl::QueryContext(egl_display,
                                       egl_context,
                                       egl::CONTEXT_CLIENT_VERSION,
                                       &mut client_version);
        assert_ne!(result, egl::FALSE);
        assert!(client_version > 0);
        let version = GLVersion::new(client_version, 0):
        println!("client version = {}", client_version);

        // Detect the config ID.
        let mut egl_config_id = 0;
        let result = egl::QueryContext(egl_display,
                                       egl_context,
                                       egl::CONFIG_ID,
                                       &mut egl_config_id);
        assert_ne!(result, egl::FALSE);

        // Fetch the current config.
        let (mut egl_config, mut egl_config_count) = (0, 0);
        let egl_config_attrs = [
            egl::CONFIG_ID as EGLint, egl_config_id,
            egl::NONE as EGLint, egl::NONE as EGLint,
            0, 0,
        ];
        let result = egl::ChooseConfig(egl_display,
                                       &egl_config_attrs[0],
                                       &mut egl_config,
                                       1,
                                       &mut egl_config_count);
        assert_ne!(result, egl::FALSE);
        assert!(egl_config_count > 0);

        // Detect pixel format.
        let alpha_size = get_config_attr(egl_display, egl_config, egl::ALPHA_SIZE);
        let depth_size = get_config_attr(egl_display, egl_config, egl::DEPTH_SIZE);
        let stencil_size = get_config_attr(egl_display, egl_config, egl::STENCIL_SIZE);

        // Convert to `surfman` context attribute flags.
        let mut attribute_flags = ContextAttributeFlags::empty();
        attribute_flags.set(ContextAttributeFlags::ALPHA, alpha_size != 0);
        attribute_flags.set(ContextAttributeFlags::DEPTH, depth_size != 0);
        attribute_flags.set(ContextAttributeFlags::STENCIL, stencil_size != 0);

        // Create appropriate context attributes.
        let attributes = ContextAttributes {
            flags: attribute_flags,
            flavor: GLFlavor { api: GLApi::GL, version },
        };

        let mut context = Context {
            egl_context,
            gl_info: GLInfo::new(&attributes),
            color_surface: ColorSurface::External,
            releaser,
        };

        if !*previous_context_created {
            gl::load_with(|symbol| {
                device.get_proc_address(&mut context, symbol).unwrap_or(ptr::null())
            });
            *previous_context_created = true;
        }

        context.gl_info.populate();
        return Ok((device, context));

        unsafe fn get_config_attr(display: EGLDisplay, config: EGLConfig, attr: EGLint) -> EGLint {
            let mut value = 0;
            let result = egl::GetConfigAttrib(display, config, attr, &mut value);
            debug_assert_ne!(result, egl::FALSE);
            value
        }
    }

    pub fn create_context(&self, attributes: &ContextAttributes) -> Result<Context, Error> {
        if attributes.flavor.api == GLApi::GLES {
            return Err(Error::UnsupportedGLType);
        }

        let mut previous_context_created = CREATE_CONTEXT_MUTEX.lock().unwrap();

        let profile = if attributes.flavor.version.major >= 3 {
            kCGLOGLPVersion_3_2_Core
        } else {
            kCGLOGLPVersion_Legacy
        };

        let pixel_format_attributes = [
            kCGLPFAOpenGLProfile, profile,
            0, 0,
        ];

        unsafe {
            let (mut pixel_format, mut pixel_format_count) = (ptr::null_mut(), 0);
            let mut err = CGLChoosePixelFormat(pixel_format_attributes.as_ptr(),
                                               &mut pixel_format,
                                               &mut pixel_format_count);
            if err != kCGLNoError {
                return Err(Error::PixelFormatSelectionFailed(err.to_windowing_api_error()));
            }
            if pixel_format_count == 0 {
                return Err(Error::NoPixelFormatFound);
            }

            let mut cgl_context = ptr::null_mut();
            err = CGLCreateContext(pixel_format, ptr::null_mut(), &mut cgl_context);
            if err != kCGLNoError {
                return Err(Error::ContextCreationFailed(err.to_windowing_api_error()));
            }

            debug_assert_ne!(cgl_context, ptr::null_mut());

            let err = CGLSetCurrentContext(cgl_context);
            if err != kCGLNoError {
                return Err(Error::MakeCurrentFailed(err.to_windowing_api_error()));
            }

            println!("Device::create_context() = {:x}", cgl_context as usize);

            let mut context = Context {
                cgl_context,
                framebuffer: Framebuffer::None,
                gl_info: GLInfo::new(attributes),
                releaser: Box::new(OwnedCGLContext),
            };

            if !*previous_context_created {
                gl::load_with(|symbol| {
                    self.get_proc_address(&mut context, symbol).unwrap_or(ptr::null())
                });
                *previous_context_created = true;
            }

            context.gl_info.populate();
            Ok(context)
        }
    }

    pub fn destroy_context(&self, context: &mut Context) -> Result<(), Error> {
        let mut result = Ok(());
        if context.cgl_context.is_null() {
            return result;
        }

        if let Framebuffer::Object {
            framebuffer_object,
            mut renderbuffers,
            color_surface_texture,
        } = mem::replace(&mut context.framebuffer, Framebuffer::None) {
            renderbuffers.destroy();

            if framebuffer_object != 0 {
                unsafe {
                    gl::DeleteFramebuffers(1, &framebuffer_object);
                }
            }

            match self.destroy_surface_texture(context, color_surface_texture) {
                Err(err) => result = Err(err),
                Ok(surface) => {
                    if let Err(err) = self.destroy_surface(context, surface) {
                        result = Err(err);
                    }
                }
            }
        }

        unsafe {
            context.releaser.release(context.cgl_context);
            context.cgl_context = ptr::null_mut();
        }

        result
    }

    #[inline]
    pub fn context_gl_info<'c>(&self, context: &'c Context) -> &'c GLInfo {
        &context.gl_info
    }

    pub fn make_context_current(&self, context: &Context) -> Result<(), Error> {
        unsafe {
            let err = CGLSetCurrentContext(context.cgl_context);
            if err != kCGLNoError {
                return Err(Error::MakeCurrentFailed(err.to_windowing_api_error()));
            }
            Ok(())
        }
    }

    pub fn make_context_not_current(&self, _: &Context) -> Result<(), Error> {
        unsafe {
            let err = CGLSetCurrentContext(ptr::null_mut());
            if err != kCGLNoError {
                return Err(Error::MakeCurrentFailed(err.to_windowing_api_error()));
            }
            Ok(())
        }
    }

    pub fn get_proc_address(&self, _: &Context, symbol_name: &str)
                            -> Result<*const c_void, Error> {
        unsafe {
            let framework_identifier: CFString =
                FromStr::from_str(OPENGL_FRAMEWORK_IDENTIFIER).unwrap();
            let framework =
                CFBundleGetBundleWithIdentifier(framework_identifier.as_concrete_TypeRef());
            if framework.is_null() {
                return Err(Error::NoGLLibraryFound);
            }

            let symbol_name: CFString = FromStr::from_str(symbol_name).unwrap();
            let fun_ptr = CFBundleGetFunctionPointerForName(framework,
                                                            symbol_name.as_concrete_TypeRef());
            if fun_ptr.is_null() {
                return Err(Error::GLFunctionNotFound);
            }
            
            return Ok(fun_ptr as *const c_void);
        }

        static OPENGL_FRAMEWORK_IDENTIFIER: &'static str = "com.apple.opengl";
    }

    #[inline]
    pub fn context_color_surface<'c>(&self, context: &'c Context) -> Option<&'c Surface> {
        match context.framebuffer {
            Framebuffer::None | Framebuffer::Window => None,
            Framebuffer::Object { ref color_surface_texture, .. } => {
                Some(&color_surface_texture.surface)
            }
        }
    }

    pub fn replace_context_color_surface(&self, context: &mut Context, new_color_surface: Surface)
                                         -> Result<Option<Surface>, Error> {
        if let Framebuffer::Window = context.framebuffer {
            return Err(Error::WindowAttached)
        }

        self.make_context_current(context)?;

        // Make sure all changes are synchronized. Apple requires this.
        unsafe {
            gl::Flush();
        }

        // Fast path: we have a FBO set up already and the sizes are the same. In this case, we can
        // just switch the backing texture.
        let can_modify_existing_framebuffer = match context.framebuffer {
            Framebuffer::Object { ref color_surface_texture, .. } => {
                // FIXME(pcwalton): Should we check parts of the descriptor other than size as
                // well?
                color_surface_texture.surface().descriptor().size ==
                    new_color_surface.descriptor().size
            }
            Framebuffer::None | Framebuffer::Window => false,
        };
        if can_modify_existing_framebuffer {
            return self.replace_color_surface_in_existing_framebuffer(context, new_color_surface)
                       .map(Some);
        }

        let (old_surface, result) = self.destroy_framebuffer(context);
        if let Err(err) = result {
            if let Some(old_surface) = old_surface {
                drop(self.destroy_surface(context, old_surface));
            }
            return Err(err);
        }
        if let Err(err) = self.create_framebuffer(context, new_color_surface) {
            if let Some(old_surface) = old_surface {
                drop(self.destroy_surface(context, old_surface));
            }
            return Err(err);
        }

        Ok(old_surface)
    }

    #[inline]
    pub fn context_surface_framebuffer_object(&self, context: &Context) -> Result<GLuint, Error> {
        match context.framebuffer {
            Framebuffer::None => Err(Error::NoSurfaceAttached),
            Framebuffer::Window => Err(Error::WindowAttached),
            Framebuffer::Object { framebuffer_object, .. } => Ok(framebuffer_object),
        }
    }

    // Assumes that the context is current.
    fn create_framebuffer(&self, context: &mut Context, color_surface: Surface)
                          -> Result<(), Error> {
        let descriptor = *color_surface.descriptor();
        let color_surface_texture = self.create_surface_texture(context, color_surface)?;

        unsafe {
            let mut framebuffer_object = 0;
            gl::GenFramebuffers(1, &mut framebuffer_object);
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer_object);

            gl::FramebufferTexture2D(gl::FRAMEBUFFER,
                                     gl::COLOR_ATTACHMENT0,
                                     SurfaceTexture::gl_texture_target(),
                                     color_surface_texture.gl_texture(),
                                     0);

            let renderbuffers = Renderbuffers::new(&descriptor.size, &context.gl_info);
            renderbuffers.bind_to_current_framebuffer();

            debug_assert_eq!(gl::CheckFramebufferStatus(gl::FRAMEBUFFER),
                             gl::FRAMEBUFFER_COMPLETE);

            // Set the viewport so that the application doesn't have to do so explicitly.
            gl::Viewport(0, 0, descriptor.size.width, descriptor.size.height);

            context.framebuffer = Framebuffer::Object {
                framebuffer_object,
                color_surface_texture,
                renderbuffers,
            };
        }

        Ok(())
    }

    fn destroy_framebuffer(&self, context: &mut Context) -> (Option<Surface>, Result<(), Error>) {
        let (framebuffer_object,
             color_surface_texture,
             mut renderbuffers) = match mem::replace(&mut context.framebuffer, Framebuffer::None) {
            Framebuffer::Window => unreachable!(),
            Framebuffer::None => return (None, Ok(())),
            Framebuffer::Object { framebuffer_object, color_surface_texture, renderbuffers } => {
                (framebuffer_object, color_surface_texture, renderbuffers)
            }
        };

        let old_surface = match self.destroy_surface_texture(context, color_surface_texture) {
            Ok(old_surface) => old_surface,
            Err(err) => return (None, Err(err)),
        };

        renderbuffers.destroy();

        unsafe {
            gl::DeleteFramebuffers(1, &framebuffer_object);
        }

        (Some(old_surface), Ok(()))
    }

    fn replace_color_surface_in_existing_framebuffer(&self,
                                                     context: &mut Context,
                                                     new_color_surface: Surface)
                                                     -> Result<Surface, Error> {
        println!("replace_color_surface_in_existing_framebuffer()");
        let new_color_surface_texture = self.create_surface_texture(context, new_color_surface)?;

        let (framebuffer_object, framebuffer_color_surface_texture) = match context.framebuffer {
            Framebuffer::Object { framebuffer_object, ref mut color_surface_texture, .. } => {
                (framebuffer_object, color_surface_texture)
            }
            _ => unreachable!(),
        };

        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer_object);
            gl::FramebufferTexture2D(gl::FRAMEBUFFER,
                                     gl::COLOR_ATTACHMENT0,
                                     SurfaceTexture::gl_texture_target(),
                                     new_color_surface_texture.gl_texture(),
                                     0);
        }

        let old_color_surface_texture = mem::replace(framebuffer_color_surface_texture,
                                                     new_color_surface_texture);
        self.destroy_surface_texture(context, old_color_surface_texture)
    }
}

struct OwnedEGLContext;

impl ReleaseContext for OwnedEGLContext {
    type Context = EGLContext;

    unsafe fn release(&mut self, cgl_context: CGLContextObj) {
        CGLSetCurrentContext(ptr::null_mut());
        CGLDestroyContext(cgl_context);
    }
}
