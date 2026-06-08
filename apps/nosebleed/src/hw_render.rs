use std::ffi::c_void;
use std::ffi::CString;

/// Minimal EGL bindings — just what we need for a headless GBM context.
mod egl {
    use std::ffi::{c_char, c_void};
    use std::ptr;

    // EGL types
    pub type EGLDisplay = *mut c_void;
    pub type EGLConfig = *mut c_void;
    pub type EGLContext = *mut c_void;
    pub type EGLSurface = *mut c_void;

    pub const EGL_NO_DISPLAY: EGLDisplay = ptr::null_mut::<c_void>();
    pub const EGL_NO_CONTEXT: EGLContext = ptr::null_mut::<c_void>();
    pub const EGL_NO_SURFACE: EGLSurface = ptr::null_mut::<c_void>();

    // EGL constants
    pub const EGL_OPENGL_ES_API: u32 = 0x30A0;
    pub const EGL_OPENGL_API: u32 = 0x30A2;
    pub const EGL_TRUE: u32 = 1;
    pub const EGL_FALSE: u32 = 0;
    pub const EGL_NONE: i32 = 0x3038;
    pub const EGL_HEIGHT: i32 = 0x3056;
    pub const EGL_WIDTH: i32 = 0x3057;
    pub const EGL_RENDERABLE_TYPE: i32 = 0x3040;
    pub const EGL_SURFACE_TYPE: i32 = 0x3033;
    pub const EGL_RED_SIZE: i32 = 0x3024;
    pub const EGL_GREEN_SIZE: i32 = 0x3023;
    pub const EGL_BLUE_SIZE: i32 = 0x3022;
    pub const EGL_ALPHA_SIZE: i32 = 0x3021;
    pub const EGL_DEPTH_SIZE: i32 = 0x3025;
    pub const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
    pub const EGL_OPENGL_BIT: i32 = 0x0008;
    pub const EGL_PBUFFER_BIT: i32 = 0x0001;
    pub const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;

    // FFI
    #[link(name = "EGL")]
    unsafe extern "C" {
        pub fn eglGetDisplay(native_display: *mut c_void) -> EGLDisplay;
        pub fn eglInitialize(dpy: EGLDisplay, major: *mut i32, minor: *mut i32) -> u32;
        pub fn eglTerminate(dpy: EGLDisplay) -> u32;
        pub fn eglChooseConfig(
            dpy: EGLDisplay, attribs: *const i32, configs: *mut EGLConfig,
            config_size: i32, num_config: *mut i32,
        ) -> u32;
        pub fn eglCreateContext(
            dpy: EGLDisplay, config: EGLConfig, share: EGLContext, attribs: *const i32,
        ) -> EGLContext;
        pub fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> u32;
        pub fn eglMakeCurrent(
            dpy: EGLDisplay, draw: EGLSurface, read: EGLSurface, ctx: EGLContext,
        ) -> u32;
        pub fn eglCreatePbufferSurface(
            dpy: EGLDisplay, config: EGLConfig, attribs: *const i32,
        ) -> EGLSurface;
        pub fn eglDestroySurface(dpy: EGLDisplay, surf: EGLSurface) -> u32;
        pub fn eglBindAPI(api: u32) -> u32;
        pub fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
        pub fn eglSwapBuffers(dpy: EGLDisplay, surf: EGLSurface) -> u32;
        pub fn eglGetError() -> u32;
    }

    pub fn get_error_str() -> String {
        format!("EGL error: 0x{:X}", unsafe { eglGetError() })
    }
}

/// Minimal GBM bindings — create a headless GPU buffer.
mod gbm {
    use std::ffi::c_void;

    pub type GbmDevice = *mut c_void;
    pub type GbmSurface = *mut c_void;

    pub const GBM_BO_USE_RENDERING: u32 = 2;

    #[link(name = "gbm")]
    unsafe extern "C" {
        pub fn gbm_create_device(fd: i32) -> GbmDevice;
        pub fn gbm_device_destroy(dev: GbmDevice);
        pub fn gbm_surface_create(
            dev: GbmDevice, width: u32, height: u32,
            format: u32, flags: u32,
        ) -> GbmSurface;
        pub fn gbm_surface_destroy(surf: GbmSurface);
    }
}

/// A headless EGL/GBM rendering context.
/// Safety: raw EGL/GBM handles are not thread-safe by default; use only from the rendering thread.
#[derive(Debug)]
pub struct HwRenderContext {
    display: egl::EGLDisplay,
    config: egl::EGLConfig,
    context: egl::EGLContext,
    surface: egl::EGLSurface,
    _gbm_device: gbm::GbmDevice,
    _gbm_surface: gbm::GbmSurface,
    pub width: u32,
    pub height: u32,
}

// SAFETY: HwRenderContext is confined to the rendering thread by convention.
// EGL/GBM handles are thread-local; the caller must ensure single-threaded access.
unsafe impl Send for HwRenderContext {}
unsafe impl Sync for HwRenderContext {}

impl HwRenderContext {
    /// Create a headless EGL context on the given DRM render node.
    pub fn create(drm_path: &str, width: u32, height: u32, context_type: u32) -> Result<Self, String> {
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(drm_path)
            .map_err(|e| format!("open {}: {}", drm_path, e))?;

        let gbm_dev = unsafe { gbm::gbm_create_device(file.as_raw_fd()) };
        if gbm_dev.is_null() {
            return Err("gbm_create_device failed".into());
        }
        std::mem::forget(file);

        let gbm_surf = unsafe {
            gbm::gbm_surface_create(gbm_dev, width, height, 0x34325258, gbm::GBM_BO_USE_RENDERING)
        };
        if gbm_surf.is_null() {
            unsafe { gbm::gbm_device_destroy(gbm_dev); }
            return Err("gbm_surface_create failed".into());
        }

        let display = unsafe { egl::eglGetDisplay(gbm_dev as *mut c_void) };
        if display == egl::EGL_NO_DISPLAY {
            Self::cleanup_gbm(gbm_dev, gbm_surf);
            return Err("eglGetDisplay failed".into());
        }

        let mut major = 0i32;
        let mut minor = 0i32;
        if unsafe { egl::eglInitialize(display, &mut major, &mut minor) } == egl::EGL_FALSE {
            Self::cleanup_all(display, gbm_dev, gbm_surf);
            return Err("eglInitialize failed".into());
        }

        let api = match context_type {
            1 => egl::EGL_OPENGL_API,
            _ => egl::EGL_OPENGL_ES_API,
        };
        if unsafe { egl::eglBindAPI(api) } == egl::EGL_FALSE {
            Self::cleanup_all(display, gbm_dev, gbm_surf);
            return Err("eglBindAPI failed".into());
        }

        let renderable_bit = match context_type {
            1 => egl::EGL_OPENGL_BIT,
            _ => egl::EGL_OPENGL_ES2_BIT,
        };
        let config_attribs = [
            egl::EGL_SURFACE_TYPE, egl::EGL_PBUFFER_BIT,
            egl::EGL_RENDERABLE_TYPE, renderable_bit,
            egl::EGL_RED_SIZE, 8,
            egl::EGL_GREEN_SIZE, 8,
            egl::EGL_BLUE_SIZE, 8,
            egl::EGL_ALPHA_SIZE, 8,
            egl::EGL_DEPTH_SIZE, 24,
            egl::EGL_NONE,
        ];
        let mut config: egl::EGLConfig = std::ptr::null_mut();
        let mut num_configs = 0i32;
        if unsafe {
            egl::eglChooseConfig(display, config_attribs.as_ptr(), &mut config, 1, &mut num_configs)
        } == egl::EGL_FALSE || num_configs == 0 {
            Self::cleanup_all(display, gbm_dev, gbm_surf);
            return Err("eglChooseConfig failed".into());
        }

        let ctx_attribs = if matches!(context_type, 2 | 4 | 5) {
            let version = if context_type == 4 { 3 } else { 2 };
            vec![egl::EGL_CONTEXT_CLIENT_VERSION, version, egl::EGL_NONE]
        } else {
            vec![egl::EGL_NONE]
        };
        let context = unsafe { egl::eglCreateContext(display, config, egl::EGL_NO_CONTEXT, ctx_attribs.as_ptr()) };
        if context == egl::EGL_NO_CONTEXT {
            Self::cleanup_all(display, gbm_dev, gbm_surf);
            return Err("eglCreateContext failed".into());
        }

        let pb_attribs = [egl::EGL_WIDTH, width as i32, egl::EGL_HEIGHT, height as i32, egl::EGL_NONE];
        let surface = unsafe { egl::eglCreatePbufferSurface(display, config, pb_attribs.as_ptr()) };
        if surface == egl::EGL_NO_SURFACE {
            Self::cleanup_all_with_ctx(display, context, gbm_dev, gbm_surf);
            return Err("eglCreatePbufferSurface failed".into());
        }

        if unsafe { egl::eglMakeCurrent(display, surface, surface, context) } == egl::EGL_FALSE {
            Self::cleanup_all_with_ctx(display, context, gbm_dev, gbm_surf);
            return Err("eglMakeCurrent failed".into());
        }

        Ok(Self {
            display,
            config,
            context,
            surface,
            _gbm_device: gbm_dev,
            _gbm_surface: gbm_surf,
            width,
            height,
        })
    }

    fn cleanup_gbm(gbm_dev: gbm::GbmDevice, gbm_surf: gbm::GbmSurface) {
        unsafe {
            if !gbm_surf.is_null() { gbm::gbm_surface_destroy(gbm_surf); }
            if !gbm_dev.is_null() { gbm::gbm_device_destroy(gbm_dev); }
        }
    }

    fn cleanup_all(display: egl::EGLDisplay, gbm_dev: gbm::GbmDevice, gbm_surf: gbm::GbmSurface) {
        unsafe { if display != egl::EGL_NO_DISPLAY { egl::eglTerminate(display); } }
        Self::cleanup_gbm(gbm_dev, gbm_surf);
    }

    fn cleanup_all_with_ctx(display: egl::EGLDisplay, context: egl::EGLContext, gbm_dev: gbm::GbmDevice, gbm_surf: gbm::GbmSurface) {
        unsafe {
            if context != egl::EGL_NO_CONTEXT { egl::eglDestroyContext(display, context); }
        }
        Self::cleanup_all(display, gbm_dev, gbm_surf);
    }

    /// glGetProcAddress via eglGetProcAddress
    pub fn get_proc_address(&self, name: &str) -> *mut c_void {
        let cname = CString::new(name).unwrap_or_default();
        unsafe { egl::eglGetProcAddress(cname.as_ptr()) }
    }

    /// Returns the framebuffer ID (always 0 for a pbuffer surface in EGL)
    pub fn get_framebuffer(&self) -> u32 { 0 }
}

impl Drop for HwRenderContext {
    fn drop(&mut self) {
        unsafe {
            egl::eglMakeCurrent(self.display, egl::EGL_NO_SURFACE, egl::EGL_NO_SURFACE, egl::EGL_NO_CONTEXT);
            if self.surface != egl::EGL_NO_SURFACE { egl::eglDestroySurface(self.display, self.surface); }
            if self.context != egl::EGL_NO_CONTEXT { egl::eglDestroyContext(self.display, self.context); }
            if self.display != egl::EGL_NO_DISPLAY { egl::eglTerminate(self.display); }
            if !self._gbm_surface.is_null() { gbm::gbm_surface_destroy(self._gbm_surface); }
            if !self._gbm_device.is_null() { gbm::gbm_device_destroy(self._gbm_device); }
        }
    }
}
