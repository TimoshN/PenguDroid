//! Bionic-compatible `libEGL` / `libGLESv2` running on the *host* GPU.
//!
//! Android apps (and their bundled native libraries) call the Bionic EGL/GLES
//! entry points directly. This crate implements the small subset they need by
//! `dlopen`ing the host's `libEGL.so.1` / `libGLESv2.so.2` at runtime — there
//! are no `-dev` headers involved — and by rendering into an offscreen FBO.
//!
//! [`eglSwapBuffers`] selects one of two independent backends. `dmabuf.rs`
//! exports the FBO texture with `EGL_MESA/EXT_image_dma_buf_export` and sends
//! `Message::PresentDmabuf`; `gralloc.rs` allocates Hybris native buffers and
//! presents mmap-able allocations through wl_shm. Both use `SCM_RIGHTS` and
//! avoid CPU readback on their normal paths.
//!
//! # Surfaces
//!
//! A Bionic app passes an `ANativeWindow*` to `eglCreateWindowSurface`, but
//! PenguDroid has no real native window: the geometry is supplied out of band
//! via `PENGUDROID_SURFACE_W` / `PENGUDROID_SURFACE_H`. The returned
//! `EGLSurface` is an opaque pointer to a [`SurfaceRec`] owned by this crate;
//! it is *not* a host EGL surface because a surfaceless display has none.
//!
//! # Global state and locking
//!
//! The host entry-point table is immutable after the first load ([`OnceLock`]).
//! Everything mutable (cached display, current surface, daemon connection and
//! surface id) lives under one [`Mutex`]. That mutex is never held across
//! arbitrary host calls; in `eglSwapBuffers` it is held for one daemon
//! request/response only (the socket has a short read timeout), so a missing or
//! wedged daemon can never hang the guest.

#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use pengudroid_proto::abi::ANDROID_RGBA_8888;
use pengudroid_proto::{default_socket_path, BufferCrop, Message, Role, PROTO_VERSION};
use pengudroid_socket::Connection;

include!(concat!(env!("OUT_DIR"), "/gl_forwarders.rs"));

// ---------------------------------------------------------------------------
// EGL/GLES types and constants
// ---------------------------------------------------------------------------

pub type EGLDisplay = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLSurface = *mut c_void;
type EGLImageKHR = *mut c_void;
type EGLClientBuffer = *mut c_void;
type EGLSync = *mut c_void;

const EGL_TRUE: u32 = 1;
const EGL_FALSE: u32 = 0;
const EGL_NOT_INITIALIZED: i32 = 0x3001;

const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
const EGL_GL_TEXTURE_2D_KHR: u32 = 0x30B1;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA: i32 = 0x1908;
const GL_RGBA8: i32 = 0x8058;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_NEAREST: i32 = 0x2600;
const GL_LINEAR: i32 = 0x2601;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_RENDERBUFFER: u32 = 0x8D41;
const GL_DEPTH24_STENCIL8: u32 = 0x88F0;
const GL_DEPTH_COMPONENT16: u32 = 0x81A5;
const GL_DEPTH_STENCIL_ATTACHMENT: u32 = 0x821A;
const GL_DEPTH_ATTACHMENT: u32 = 0x8D00;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_SCISSOR_TEST: u32 = 0x0C11;
const GL_SCISSOR_BOX: u32 = 0x0C10;
const GL_COLOR_CLEAR_VALUE: u32 = 0x0C22;
const GL_COLOR_WRITEMASK: u32 = 0x0C23;
const GL_COLOR_BUFFER_BIT: u32 = 0x00004000;

// ---------------------------------------------------------------------------
// Host entry-point signatures
// ---------------------------------------------------------------------------

type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type EglGetDisplay = unsafe extern "C" fn(*mut c_void) -> EGLDisplay;
type EglGetPlatformDisplay =
    unsafe extern "C" fn(u32, *mut c_void, *const isize) -> EGLDisplay;
type EglGetPlatformDisplayEXT =
    unsafe extern "C" fn(u32, *mut c_void, *const c_int) -> EGLDisplay;
type EglInitialize = unsafe extern "C" fn(EGLDisplay, *mut c_int, *mut c_int) -> u32;
type EglTerminate = unsafe extern "C" fn(EGLDisplay) -> u32;
type EglQueryString = unsafe extern "C" fn(EGLDisplay, c_int) -> *const c_char;
type EglGetError = unsafe extern "C" fn() -> c_int;
type EglGetConfigs =
    unsafe extern "C" fn(EGLDisplay, *mut EGLConfig, c_int, *mut c_int) -> u32;
type EglChooseConfig =
    unsafe extern "C" fn(EGLDisplay, *const c_int, *mut EGLConfig, c_int, *mut c_int) -> u32;
type EglGetConfigAttrib = unsafe extern "C" fn(EGLDisplay, EGLConfig, c_int, *mut c_int) -> u32;
type EglBindTexImage = unsafe extern "C" fn(EGLDisplay, EGLSurface, c_int) -> u32;
type EglClientWaitSync = unsafe extern "C" fn(EGLDisplay, EGLSync, c_int, u64) -> c_int;
type EglCreatePbufferFromClientBuffer = unsafe extern "C" fn(EGLDisplay, c_int, EGLClientBuffer, EGLConfig, *const c_int) -> EGLSurface;
type EglCreateSync = unsafe extern "C" fn(EGLDisplay, c_int, *const isize) -> EGLSync;
type EglDestroySync = unsafe extern "C" fn(EGLDisplay, EGLSync) -> u32;
type EglGetSyncAttrib = unsafe extern "C" fn(EGLDisplay, EGLSync, c_int, *mut isize) -> u32;
type EglQueryContext = unsafe extern "C" fn(EGLDisplay, EGLContext, c_int, *mut c_int) -> u32;
type EglWaitNoArgs = unsafe extern "C" fn() -> u32;
type EglWaitNative = unsafe extern "C" fn(c_int) -> u32;
type EglWaitSync = unsafe extern "C" fn(EGLDisplay, EGLSync, c_int) -> u32;
type EglCreateContext =
    unsafe extern "C" fn(EGLDisplay, EGLConfig, EGLContext, *const c_int) -> EGLContext;
type EglDestroyContext = unsafe extern "C" fn(EGLDisplay, EGLContext) -> u32;
type EglMakeCurrent =
    unsafe extern "C" fn(EGLDisplay, EGLSurface, EGLSurface, EGLContext) -> u32;
type EglGetCurrentContext = unsafe extern "C" fn() -> EGLContext;
type EglGetCurrentDisplay = unsafe extern "C" fn() -> EGLDisplay;
type EglBindAPI = unsafe extern "C" fn(u32) -> u32;
type EglSwapInterval = unsafe extern "C" fn(EGLDisplay, c_int) -> u32;
type EglCreateImageKHR =
    unsafe extern "C" fn(EGLDisplay, EGLContext, u32, EGLClientBuffer, *const c_int) -> EGLImageKHR;
type EglDestroyImageKHR = unsafe extern "C" fn(EGLDisplay, EGLImageKHR) -> u32;
type EglExportDMABUFImageQueryMESA =
    unsafe extern "C" fn(EGLDisplay, EGLImageKHR, *mut c_int, *mut c_int, *mut u64) -> u32;
type EglExportDMABUFImageMESA =
    unsafe extern "C" fn(EGLDisplay, EGLImageKHR, *mut c_int, *mut c_int, *mut c_int) -> u32;

type GlGenTextures = unsafe extern "C" fn(c_int, *mut u32);
type GlBindTexture = unsafe extern "C" fn(u32, u32);
type GlTexImage2D = unsafe extern "C" fn(
    u32,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    u32,
    u32,
    *const c_void,
);
type GlTexParameteri = unsafe extern "C" fn(u32, u32, c_int);
type GlGenFramebuffers = unsafe extern "C" fn(c_int, *mut u32);
type GlBindFramebuffer = unsafe extern "C" fn(u32, u32);
type GlFramebufferTexture2D = unsafe extern "C" fn(u32, u32, u32, u32, c_int);
type GlViewport = unsafe extern "C" fn(c_int, c_int, c_int, c_int);
type GlGetError = unsafe extern "C" fn() -> u32;
type GlGetString = unsafe extern "C" fn(u32) -> *const u8;
type GlClear = unsafe extern "C" fn(u32);
type GlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type GlFinish = unsafe extern "C" fn();
type GlFlush = unsafe extern "C" fn();
type GlReadPixels = unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void);
type GlEGLImageTargetTexture2D = unsafe extern "C" fn(u32, EGLImageKHR);
type GlBlitFramebuffer = unsafe extern "C" fn(
    i32, i32, i32, i32, i32, i32, i32, i32, u32, u32,
);
type GlGenRenderbuffers = unsafe extern "C" fn(c_int, *mut u32);
type GlBindRenderbuffer = unsafe extern "C" fn(u32, u32);
type GlRenderbufferStorage = unsafe extern "C" fn(u32, u32, c_int, c_int);
type GlFramebufferRenderbuffer = unsafe extern "C" fn(u32, u32, u32, u32);
type GlDeleteRenderbuffers = unsafe extern "C" fn(c_int, *const u32);
type GlCheckFramebufferStatus = unsafe extern "C" fn(u32) -> u32;
type GlEnable = unsafe extern "C" fn(u32);
type GlDisable = unsafe extern "C" fn(u32);
type GlIsEnabled = unsafe extern "C" fn(u32) -> u8;
type GlScissor = unsafe extern "C" fn(c_int, c_int, c_int, c_int);
type GlGetIntegerv = unsafe extern "C" fn(u32, *mut c_int);
type GlGetFloatv = unsafe extern "C" fn(u32, *mut f32);
type GlGetBooleanv = unsafe extern "C" fn(u32, *mut u8);
type GlColorMask = unsafe extern "C" fn(u8, u8, u8, u8);

// ---------------------------------------------------------------------------
// Host table
// ---------------------------------------------------------------------------

struct Host {
    /// `libGLESv2` handle, kept for the `eglGetProcAddress` dlsym fallback.
    gles_handle: usize,
    gl_namespace: libc::Lmid_t,
    gl_isolated: bool,

    get_proc_address: Option<EglGetProcAddress>,
    get_display: EglGetDisplay,
    get_platform_display: Option<EglGetPlatformDisplay>,
    get_platform_display_ext: Option<EglGetPlatformDisplayEXT>,
    initialize: EglInitialize,
    terminate: EglTerminate,
    query_string: EglQueryString,
    get_error: EglGetError,
    get_configs: EglGetConfigs,
    choose_config: EglChooseConfig,
    get_config_attrib: EglGetConfigAttrib,
    bind_tex_image: Option<EglBindTexImage>,
    release_tex_image: Option<EglBindTexImage>,
    client_wait_sync: Option<EglClientWaitSync>,
    create_pbuffer_from_client_buffer: Option<EglCreatePbufferFromClientBuffer>,
    create_sync: Option<EglCreateSync>,
    destroy_sync: Option<EglDestroySync>,
    get_sync_attrib: Option<EglGetSyncAttrib>,
    query_context: Option<EglQueryContext>,
    wait_client: Option<EglWaitNoArgs>,
    wait_gl: Option<EglWaitNoArgs>,
    wait_native: Option<EglWaitNative>,
    wait_sync: Option<EglWaitSync>,
    create_context: EglCreateContext,
    destroy_context: EglDestroyContext,
    make_current: EglMakeCurrent,
    get_current_context: Option<EglGetCurrentContext>,
    get_current_display: Option<EglGetCurrentDisplay>,
    bind_api: Option<EglBindAPI>,
    swap_interval: Option<EglSwapInterval>,
    create_image_khr: Option<EglCreateImageKHR>,
    destroy_image_khr: Option<EglDestroyImageKHR>,
    export_dmabuf_query: Option<EglExportDMABUFImageQueryMESA>,
    export_dmabuf_export: Option<EglExportDMABUFImageMESA>,

    gl_egl_image_target_texture_2d: Option<GlEGLImageTargetTexture2D>,
    gl_blit_framebuffer: Option<GlBlitFramebuffer>,
    gl_gen_textures: Option<GlGenTextures>,
    gl_bind_texture: Option<GlBindTexture>,
    gl_tex_image_2d: Option<GlTexImage2D>,
    gl_tex_parameter_i: Option<GlTexParameteri>,
    gl_gen_framebuffers: Option<GlGenFramebuffers>,
    gl_bind_framebuffer: Option<GlBindFramebuffer>,
    gl_framebuffer_texture_2d: Option<GlFramebufferTexture2D>,
    gl_viewport: Option<GlViewport>,
    gl_get_error: Option<GlGetError>,
    gl_get_string: Option<GlGetString>,
    gl_clear: Option<GlClear>,
    gl_clear_color: Option<GlClearColor>,
    gl_finish: Option<GlFinish>,
    gl_flush: Option<GlFlush>,
    gl_read_pixels: Option<GlReadPixels>,
    gl_gen_renderbuffers: Option<GlGenRenderbuffers>,
    gl_bind_renderbuffer: Option<GlBindRenderbuffer>,
    gl_renderbuffer_storage: Option<GlRenderbufferStorage>,
    gl_framebuffer_renderbuffer: Option<GlFramebufferRenderbuffer>,
    gl_delete_renderbuffers: Option<GlDeleteRenderbuffers>,
    gl_check_framebuffer_status: Option<GlCheckFramebufferStatus>,
    gl_enable: Option<GlEnable>,
    gl_disable: Option<GlDisable>,
    gl_is_enabled: Option<GlIsEnabled>,
    gl_scissor: Option<GlScissor>,
    gl_get_integerv: Option<GlGetIntegerv>,
    gl_get_floatv: Option<GlGetFloatv>,
    gl_get_booleanv: Option<GlGetBooleanv>,
    gl_color_mask: Option<GlColorMask>,
}

thread_local! {
    static HOST_LOADING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static HOST_CALL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Non-zero only while this thread is executing a host EGL/GLES call.
///
/// The Android compatibility shim uses this boundary to keep Hybris' own
/// hwbinder/configstore traffic on the real device, while the guest ART
/// framework continues to use PenguDroid's in-process binder broker.
#[no_mangle]
pub extern "C" fn __pengudroid_host_gl_active() -> c_int {
    HOST_CALL_DEPTH.with(|depth| depth.get() as c_int)
}

struct HostCallGuard;

impl HostCallGuard {
    fn enter() -> Self {
        HOST_CALL_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for HostCallGuard {
    fn drop(&mut self) {
        HOST_CALL_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

impl Host {
    fn load() -> Result<Host, String> {
        struct LoadingGuard;
        impl Drop for LoadingGuard {
            fn drop(&mut self) {
                HOST_LOADING.with(|loading| loading.set(false));
            }
        }
        HOST_LOADING.with(|loading| loading.set(true));
        let _loading_guard = LoadingGuard;
        let isolated = std::env::var_os("PENGUDROID_HOST_GL_NAMESPACE").is_some()
            || hybris_host();
        let mut namespace = libc::LM_ID_NEWLM;
        // libEGL's static initializer may query configstore immediately. Set
        // Hybris' SDR properties before that dlopen, not after it.
        if isolated && hybris_host() {
            configure_hybris_sdr_properties(&mut namespace);
        }
        let egl_handle = open_first(
            &["libEGL.so.1", "libEGL.so", "libEGL_mesa.so.0"],
            isolated,
            namespace,
        )
        .ok_or_else(|| "could not dlopen libEGL.so.1".to_string())?;
        if isolated {
            // Both handles must share one namespace: EGL and GLES have to see
            // the same Hybris vendor driver and its TLS/state. dlinfo returns
            // the namespace allocated for the first dlmopen call.
            if unsafe { libc::dlinfo(
                egl_handle,
                libc::RTLD_DI_LMID,
                &mut namespace as *mut libc::Lmid_t as *mut c_void,
            ) } != 0 {
                return Err("could not query the isolated host EGL namespace".to_string());
            }
        }
        let gles_handle = open_first(
            &["libGLESv2.so.2", "libGLESv2.so"],
            isolated,
            namespace,
        )
        .ok_or_else(|| "could not dlopen libGLESv2.so.2".to_string())?;

        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            trace_mapped_gl();
        }
        // Debug aid: let an unrelated gdb attach under Yama ptrace_scope=1.
        if std::env::var_os("PENGUDROID_ALLOW_PTRACE").is_some() {
            // SAFETY: prctl with integer arguments only.
            unsafe { libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0) };
            eprintln!("[gleshim] ptrace allowed for pid {}", std::process::id());
        }

        // Core entry points are looked up in libEGL itself; the extension
        // entry points may only be published through `eglGetProcAddress`.
        let core = |name: &str| sym_ptr(egl_handle, name);

        let get_proc_address: Option<EglGetProcAddress> =
            core("eglGetProcAddress").and_then(cast_fn);

        let proc_or_sym = |name: &str| -> Option<*mut c_void> {
            if let Some(get_proc_address) = get_proc_address {
                if let Ok(c_name) = CString::new(name) {
                    // SAFETY: `get_proc_address` is the host EGL entry point and
                    // `c_name` is a valid NUL-terminated string.
                    let ptr = unsafe { get_proc_address(c_name.as_ptr()) };
                    if !ptr.is_null() {
                        return Some(ptr);
                    }
                }
            }
            sym_ptr(egl_handle, name)
        };

        let gl_proc = |name: &str| -> Option<*mut c_void> {
            if let Some(get_proc_address) = get_proc_address {
                if let Ok(c_name) = CString::new(name) {
                    // SAFETY: as above.
                    let ptr = unsafe { get_proc_address(c_name.as_ptr()) };
                    if !ptr.is_null() {
                        return Some(ptr);
                    }
                }
            }
            sym_ptr(gles_handle, name).or_else(|| sym_ptr(egl_handle, name))
        };

        // SAFETY: the assembly table is writable, process-lifetime storage;
        // each entry is initialized once before guest GL calls begin.
        unsafe { init_host_gl_forwarders(gles_handle, get_proc_address) };

        Ok(Host {
            gles_handle: gles_handle as usize,
            gl_namespace: namespace,
            gl_isolated: isolated,
            get_proc_address,
            get_display: need(core("eglGetDisplay"), "eglGetDisplay")?,
            get_platform_display: core("eglGetPlatformDisplay").and_then(cast_fn),
            get_platform_display_ext: proc_or_sym("eglGetPlatformDisplayEXT")
                .and_then(cast_fn),
            initialize: need(core("eglInitialize"), "eglInitialize")?,
            terminate: need(core("eglTerminate"), "eglTerminate")?,
            query_string: need(core("eglQueryString"), "eglQueryString")?,
            get_error: need(core("eglGetError"), "eglGetError")?,
            get_configs: need(core("eglGetConfigs"), "eglGetConfigs")?,
            choose_config: need(core("eglChooseConfig"), "eglChooseConfig")?,
            get_config_attrib: need(core("eglGetConfigAttrib"), "eglGetConfigAttrib")?,
            bind_tex_image: core("eglBindTexImage").and_then(cast_fn),
            release_tex_image: core("eglReleaseTexImage").and_then(cast_fn),
            client_wait_sync: core("eglClientWaitSync")
                .or_else(|| proc_or_sym("eglClientWaitSyncKHR"))
                .and_then(cast_fn),
            create_pbuffer_from_client_buffer: core("eglCreatePbufferFromClientBuffer").and_then(cast_fn),
            // EGL 1.5 core names may be absent from the loaded libEGL even
            // when the driver supports EGL_KHR_fence_sync (the ARM32 Hybris
            // wrapper libEGL exports almost nothing itself). The KHR-suffix
            // entry points are semantically identical for fence syncs.
            create_sync: core("eglCreateSync")
                .or_else(|| proc_or_sym("eglCreateSyncKHR"))
                .and_then(cast_fn),
            destroy_sync: core("eglDestroySync")
                .or_else(|| proc_or_sym("eglDestroySyncKHR"))
                .and_then(cast_fn),
            get_sync_attrib: core("eglGetSyncAttrib")
                .or_else(|| proc_or_sym("eglGetSyncAttribKHR"))
                .and_then(cast_fn),
            query_context: core("eglQueryContext").and_then(cast_fn),
            wait_client: core("eglWaitClient").and_then(cast_fn),
            wait_gl: core("eglWaitGL").and_then(cast_fn),
            wait_native: core("eglWaitNative").and_then(cast_fn),
            wait_sync: core("eglWaitSync").and_then(cast_fn),
            create_context: need(core("eglCreateContext"), "eglCreateContext")?,
            destroy_context: need(core("eglDestroyContext"), "eglDestroyContext")?,
            make_current: need(core("eglMakeCurrent"), "eglMakeCurrent")?,
            get_current_context: core("eglGetCurrentContext").and_then(cast_fn),
            get_current_display: core("eglGetCurrentDisplay").and_then(cast_fn),
            bind_api: core("eglBindAPI").and_then(cast_fn),
            swap_interval: core("eglSwapInterval").and_then(cast_fn),
            create_image_khr: proc_or_sym("eglCreateImageKHR").and_then(cast_fn),
            destroy_image_khr: proc_or_sym("eglDestroyImageKHR").and_then(cast_fn),
            // Mesa and the EXT extension have the same ABI. Mali/Hybris may
            // expose only the EXT entry points, so do not require MESA names.
            export_dmabuf_query: proc_or_sym("eglExportDMABUFImageQueryMESA")
                .or_else(|| proc_or_sym("eglExportDMABUFImageQueryEXT"))
                .and_then(cast_fn),
            export_dmabuf_export: proc_or_sym("eglExportDMABUFImageMESA")
                .or_else(|| proc_or_sym("eglExportDMABUFImageEXT"))
                .and_then(cast_fn),

            gl_egl_image_target_texture_2d: gl_proc("glEGLImageTargetTexture2DOES").and_then(cast_fn),
            gl_blit_framebuffer: gl_proc("glBlitFramebuffer").and_then(cast_fn),
            gl_gen_textures: gl_proc("glGenTextures").and_then(cast_fn),
            gl_bind_texture: gl_proc("glBindTexture").and_then(cast_fn),
            gl_tex_image_2d: gl_proc("glTexImage2D").and_then(cast_fn),
            gl_tex_parameter_i: gl_proc("glTexParameteri").and_then(cast_fn),
            gl_gen_framebuffers: gl_proc("glGenFramebuffers").and_then(cast_fn),
            gl_bind_framebuffer: gl_proc("glBindFramebuffer").and_then(cast_fn),
            gl_framebuffer_texture_2d: gl_proc("glFramebufferTexture2D")
                .and_then(cast_fn),
            gl_viewport: gl_proc("glViewport").and_then(cast_fn),
            gl_get_error: gl_proc("glGetError").and_then(cast_fn),
            gl_get_string: gl_proc("glGetString").and_then(cast_fn),
            gl_clear: gl_proc("glClear").and_then(cast_fn),
            gl_clear_color: gl_proc("glClearColor").and_then(cast_fn),
            gl_finish: gl_proc("glFinish").and_then(cast_fn),
            gl_flush: gl_proc("glFlush").and_then(cast_fn),
            gl_read_pixels: gl_proc("glReadPixels").and_then(cast_fn),
            gl_gen_renderbuffers: gl_proc("glGenRenderbuffers").and_then(cast_fn),
            gl_bind_renderbuffer: gl_proc("glBindRenderbuffer").and_then(cast_fn),
            gl_renderbuffer_storage: gl_proc("glRenderbufferStorage").and_then(cast_fn),
            gl_framebuffer_renderbuffer: gl_proc("glFramebufferRenderbuffer").and_then(cast_fn),
            gl_delete_renderbuffers: gl_proc("glDeleteRenderbuffers").and_then(cast_fn),
            gl_check_framebuffer_status: gl_proc("glCheckFramebufferStatus").and_then(cast_fn),
            gl_enable: gl_proc("glEnable").and_then(cast_fn),
            gl_disable: gl_proc("glDisable").and_then(cast_fn),
            gl_is_enabled: gl_proc("glIsEnabled").and_then(cast_fn),
            gl_scissor: gl_proc("glScissor").and_then(cast_fn),
            gl_get_integerv: gl_proc("glGetIntegerv").and_then(cast_fn),
            gl_get_floatv: gl_proc("glGetFloatv").and_then(cast_fn),
            gl_get_booleanv: gl_proc("glGetBooleanv").and_then(cast_fn),
            gl_color_mask: gl_proc("glColorMask").and_then(cast_fn),
        })
    }
}

static HOST: OnceLock<Result<Host, String>> = OnceLock::new();

/// Load the host driver at process start.
///
/// An app library is `dlopen`ed with `RTLD_NOW`, so every GLES symbol it imports
/// must already be resolvable. Loading the host driver here (before any app
/// code runs) makes its symbols global in time; doing it lazily on the first EGL
/// call would be too late.
#[used]
#[link_section = ".init_array"]
static PENGUDROID_GLESHIM_CTOR: extern "C" fn() = gleshim_init;

extern "C" fn gleshim_init() {
    // Aurora's Hybris EGL can itself call back into the Android linker while
    // resolving host entry points. Doing that from this constructor nests one
    // loader operation inside another and can deadlock before main. An explicit
    // lazy mode postpones host loading until the first guest EGL call.
    if std::env::var_os("PENGUDROID_LAZY_HOST_GL").is_none() && !hybris_host() {
        let _ = host();
    }
}

fn hybris_host() -> bool {
    if std::env::var_os("PENGUDROID_DISABLE_HYBRIS").is_some() {
        return false;
    }
    let egl = if cfg!(target_arch = "arm") {
        "/usr/libexec/droid-hybris/system/lib/libEGL.so"
    } else {
        "/usr/libexec/droid-hybris/system/lib64/libEGL.so"
    };
    cfg!(any(target_arch = "aarch64", target_arch = "arm"))
        && std::path::Path::new("/usr/lib/libhybris-common.so.1").is_file()
        && std::path::Path::new(egl).is_file()
}

/// Watchdog: if the daemon dies without cleaning up (e.g. SIGKILL), the guest
/// must not keep running orphaned. Probe the control socket periodically and
/// exit once the daemon is gone for good.
fn start_daemon_watchdog() {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        std::thread::spawn(|| {
            let mut failures = 0u32;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                match Connection::connect_endpoint(&default_socket_path()) {
                    Ok(_) => failures = 0,
                    Err(_) => {
                        failures += 1;
                        if failures >= 3 {
                            eprintln!("[gleshim] daemon disappeared; terminating orphaned guest");
                            unsafe { libc::_exit(0); }
                        }
                    }
                }
            }
        });
    });
}

fn host() -> Result<&'static Host, String> {
    HOST.get_or_init(|| {
        start_daemon_watchdog();
        Host::load()
    }).as_ref().map_err(Clone::clone)
}

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
fn mapped_elf_base(path: &str) -> Option<usize> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let range = fields.next()?;
        let _permissions = fields.next()?;
        let offset = fields.next()?;
        if offset != "00000000" || !line.ends_with(path) { continue; }
        let start = range.split('-').next()?;
        return usize::from_str_radix(start, 16).ok();
    }
    None
}

mod dmabuf;
mod gralloc;
mod hybris_tls;

pub use dmabuf::{export_surface_dmabuf, DmabufExport};

/// Publish the hosted SDR configuration before loading Hybris EGL. Loading
/// libhybris-common first also gives EGL/GLES the same isolated namespace.
#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
fn configure_hybris_sdr_properties(namespace: &mut libc::Lmid_t) {
    // Hybris brings its own Bionic libc into this isolated namespace. Without
    // this bridge, Bionic and glibc independently allocate identical pthread
    // keys: Hybris eglGetError can then overwrite Android IPCThreadState's
    // mProcess through a shared TLS slot. Load only the tiny pthread boundary
    // first, then retain that namespace for libhybris-common and EGL/GLES.
    if let Some(gleshim) = std::env::var_os("PENGUDROID_GLESHIM") {
        let path = std::path::PathBuf::from(gleshim)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("libpengudroid_hybris_pthread.so");
        if let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) {
            let bridge = unsafe {
                libc::dlmopen(
                    *namespace,
                    c_path.as_ptr(),
                    libc::RTLD_NOW | libc::RTLD_LOCAL,
                )
            };
            if !bridge.is_null() {
                if unsafe {
                    libc::dlinfo(
                        bridge,
                        libc::RTLD_DI_LMID,
                        namespace as *mut libc::Lmid_t as *mut c_void,
                    )
                } == 0 && std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
                    eprintln!("[gleshim] loaded Hybris pthread bridge in ns={namespace}");
                }
            } else if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
                eprintln!("[gleshim] could not load Hybris pthread bridge {}", path.display());
            }
        }
    }
    let path = c"/usr/lib/libhybris-common.so.1";
    // The handle deliberately remains open for the process lifetime, like the
    // subsequent EGL/GLES handles.
    let common = unsafe {
        libc::dlmopen(
            *namespace,
            path.as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        )
    };
    if common.is_null() {
        return;
    }
    if unsafe {
        libc::dlinfo(
            common,
            libc::RTLD_DI_LMID,
            namespace as *mut libc::Lmid_t as *mut c_void,
        )
    } != 0 {
        return;
    }
    type PropertySet = unsafe extern "C" fn(*const c_char, *const c_char) -> c_int;
    let Some(set) = sym_ptr(common, "my_property_set").and_then(cast_fn) else {
        return;
    };
    let set: PropertySet = set;
    for key in [
        "ro.surface_flinger.has_wide_color_display",
        "ro.surface_flinger.has_HDR_display",
        "ro.surface_flinger.use_context_priority",
    ] {
        let key = CString::new(key).unwrap();
        // SAFETY: my_property_set is libhybris-common's ordinary (key, value)
        // setter; both strings are valid and remain alive for the call.
        unsafe { set(key.as_ptr(), c"false".as_ptr()) };
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] configured Hybris SDR properties before host EGL");
    }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
fn configure_hybris_sdr_properties(_namespace: &mut libc::Lmid_t) {}

fn open_first(names: &[&str], isolated: bool, namespace: libc::Lmid_t) -> Option<*mut c_void> {
    for name in names {
        // An isolated Hybris load must not accidentally pick the guest
        // libEGL.so symlink from LD_LIBRARY_PATH. Absolute host paths also
        // make the operation independent of the prepared ART sysroot.
        let host_path = if isolated && names[0].starts_with("libEGL") {
            std::env::var("PENGUDROID_HOST_EGL").ok().or_else(|| {
                Some(match *name {
                    "libEGL.so.1" => "/usr/lib/libEGL.so.1".to_string(),
                    "libEGL.so" => "/usr/lib/libEGL.so".to_string(),
                    _ => "/usr/lib/libEGL_mesa.so.0".to_string(),
                })
            })
        } else if isolated {
            std::env::var("PENGUDROID_HOST_GLES").ok().or_else(|| {
                Some(if *name == "libGLESv2.so.2" {
                    "/usr/lib/libGLESv2.so.2".to_string()
                } else {
                    "/usr/lib/libGLESv2.so".to_string()
                })
            })
        } else {
            Some((*name).to_string())
        };
        let Some(path) = host_path else { continue };
        if let Ok(c_name) = CString::new(path) {
            // RTLD_GLOBAL: an app's native library imports the whole GLES2 API
            // (glGenBuffers, glDrawElements, ...), not just the handful this
            // shim wraps. Making the host driver's symbols global lets those
            // resolve directly; the shim's own exports still interpose first
            // because it is preloaded.
            #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
            let flags = if isolated {
                // glibc rejects RTLD_DEEPBIND in dlmopen mode. A separate
                // namespace already supplies the isolation we need here.
                libc::RTLD_NOW | libc::RTLD_LOCAL
            } else {
                libc::RTLD_NOW | libc::RTLD_GLOBAL | libc::RTLD_DEEPBIND
            };
            #[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
            let flags = libc::RTLD_NOW | libc::RTLD_GLOBAL;
            // SAFETY: `c_name` is a valid NUL-terminated soname/path. dlmopen
            // keeps Hybris' android_dlopen and public EGL references out of the
            // guest's global symbol scope.
            let handle = unsafe {
                if isolated {
                    libc::dlmopen(namespace, c_name.as_ptr(), flags)
                } else {
                    libc::dlopen(c_name.as_ptr(), flags)
                }
            };
            if !handle.is_null() {
                if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
                    eprintln!("[gleshim] host loaded {:?} isolated={isolated} ns={namespace}", c_name);
                }
                return Some(handle);
            }
            if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
                let error = unsafe { libc::dlerror() };
                if !error.is_null() {
                    eprintln!("[gleshim] host load {:?}: {}", c_name, unsafe { std::ffi::CStr::from_ptr(error) }.to_string_lossy());
                }
            }
        }
    }
    None
}

/// Print every mapped `libEGL`/`libGLESv2` image, for diagnostics.
///
/// `PENGUDROID_TRACE_GLESHIM=1` runs call this once. It answers the question the
/// symbol-level debugging cannot: *which* `libEGL` the process actually mapped,
/// the host's or the Bionic tree's copy that `nativeloader` also pulls in.
fn trace_mapped_gl() {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return;
    };
    let mut seen = std::collections::BTreeSet::new();
    for line in maps.lines() {
        let Some(path) = line.split_whitespace().last() else { continue };
        if !(path.contains("libEGL") || path.contains("libGLES")) {
            continue;
        }
        if seen.insert(path.to_string()) {
            eprintln!("[gleshim] mapped {path}");
        }
    }
}

fn sym_ptr(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let c_name = CString::new(name).ok()?;
    // SAFETY: `handle` came from `dlopen` and `c_name` is a valid C string.
    let ptr = unsafe { libc::dlsym(handle, c_name.as_ptr()) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

fn need<T: Copy>(ptr: Option<*mut c_void>, name: &str) -> Result<T, String> {
    ptr.and_then(cast_fn)
        .ok_or_else(|| format!("host symbol `{name}` is missing"))
}

/// Reinterpret a `dlsym`/`eglGetProcAddress` result as a function pointer.
///
/// The caller only instantiates `T` with an `unsafe extern "C" fn` type that
/// matches the symbol the pointer was looked up under. POSIX guarantees the
/// representation of `void *` matches a function pointer on Linux.
fn cast_fn<T: Copy>(ptr: *mut c_void) -> Option<T> {
    if ptr.is_null() {
        return None;
    }
    debug_assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*mut c_void>()
    );
    // SAFETY: see the function comment.
    Some(unsafe { std::mem::transmute_copy(&ptr) })
}

// ---------------------------------------------------------------------------
// Mutable global state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    /// Host `EGLDisplay`, stored as `usize` so `State` stays `Send`.
    display: usize,
    /// Host `EGLContext` that some thread most recently made current. Only a
    /// fallback for a thread that has none; what is current is per thread.
    context: usize,

    conn: Option<Connection>,
    surface_id: Option<u32>,
    /// Whether we already tried (and possibly failed) to reach the daemon.
    connect_attempted: bool,
}

// EGL's current context and draw surface are per *thread*. They used to be one
// process-wide value, so when hwui's RenderThread made its surface current, a
// game's SDL render thread had its `glBindFramebuffer(0)` redirected to hwui's
// FBO name, which does not exist in the game's context: every frame went
// nowhere and the window stayed black. Which thread made its surface current
// last depended on scheduling, so the same APK rendered on some runs and was
// black on others (measured on Beach Buggy Racing 2).
thread_local! {
    static CURRENT_CONTEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CURRENT_SURFACE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Every `SurfaceRec` this module created and has not destroyed. Another
/// thread may still name a surface as current after this thread destroyed it;
/// it is only dereferenced while it is in here.
fn live_surfaces() -> &'static Mutex<std::collections::HashSet<usize>> {
    static LIVE: OnceLock<Mutex<std::collections::HashSet<usize>>> = OnceLock::new();
    LIVE.get_or_init(Default::default)
}

fn current_surface() -> usize {
    let surface = CURRENT_SURFACE.with(|c| c.get());
    if surface != 0 && !live_surfaces().lock().unwrap_or_else(|e| e.into_inner()).contains(&surface) {
        CURRENT_SURFACE.with(|c| c.set(0));
        return 0;
    }
    surface
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn lock_state() -> MutexGuard<'static, State> {
    // A panic in another FFI call poisons the mutex; recover so a guest never
    // sees a hard failure because of it.
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// Every EGL attribute list in practice is a handful of pairs; a caller that
/// has not written `EGL_NONE` by then is not passing a list this shim can read.
const MAX_ATTRIB_PAIRS: usize = 64;
const EGL_NONE: c_int = 0x3038;

/// Copy an `EGL_NONE`-terminated attribute list into a `Vec`.
///
/// Returns `None` when the caller's list is unterminated. That case used to be
/// an unbounded `loop` that read past the end of the mapping forever: a guest
/// with a stale or mis-translated pointer left the render thread spinning at
/// 100% CPU with no syscalls, which is indistinguishable from a hang. Callers
/// treat `None` as "forward the original pointer unchanged".
unsafe fn read_attribs(attribs: *const c_int) -> Option<Vec<c_int>> {
    if attribs.is_null() {
        return None;
    }
    let mut pairs = Vec::new();
    for index in 0..MAX_ATTRIB_PAIRS {
        let key = *attribs.add(index * 2);
        if key == EGL_NONE {
            pairs.push(EGL_NONE);
            return Some(pairs);
        }
        pairs.push(key);
        pairs.push(*attribs.add(index * 2 + 1));
    }
    log_once(format_args!(
        "EGL attribute list at {attribs:p} has no EGL_NONE terminator; forwarding it unchanged"
    ));
    None
}

static LOGGED: AtomicBool = AtomicBool::new(false);

/// Emit the first diagnostic only. Bytecode loaders call EGL from many threads
/// and a missing daemon would otherwise flood the guest's stderr.
fn log_once(args: std::fmt::Arguments<'_>) {
    if !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("[pengudroid-gleshim] {args}");
    }
}

// ---------------------------------------------------------------------------
// Surfaces and FBOs
// ---------------------------------------------------------------------------

/// The object behind every `EGLSurface` this crate hands out.
struct SurfaceRec {
    w: u32,
    h: u32,
    /// The colour texture for each GL share group that drew into this surface,
    /// and per context the FBO that wraps it. Textures are shared within a
    /// share group but framebuffer objects never are: one FBO name per surface
    /// was bound in every context that made the surface current, and in an
    /// unrelated context (Chromium's GPU thread inside a WebView) that name
    /// does not exist, so `glBindFramebuffer(0)` raised GL_INVALID_OPERATION and
    /// Chromium aborted.
    targets: Vec<Target>,
    /// The daemon's surface id for this window, or 0 until the first present.
    ///
    /// It lives here rather than on the shared `State` because the framework
    /// gives each window its own EGL surface: an activity and a dialog on the
    /// same render thread. A single process-wide id made both windows present
    /// into one Wayland surface, so they overwrote each other and only whichever
    /// drew last was visible.
    surface_id: u32,
    /// Only surfaces created here may be destroyed here; injected ids are borrowed.
    owns_surface: bool,
    native_window: usize,
    layer_resolved: bool,
    child_of_early: bool,
}

struct Target {
    group: u64,
    tex: u32,
    depth_rbo: u32,
    gralloc: Option<gralloc::GrallocTarget>,
    flip: Option<gralloc::GrallocFlip>,
    /// `(context serial, fbo)`.
    fbos: Vec<(u64, u32)>,
}

/// What gleshim knows about a context: a serial unique to this creation (the
/// driver may hand the same handle out again after a destroy) and the serial
/// of its share group's first context.
#[derive(Clone, Copy)]
struct ContextInfo { serial: u64, group: u64 }

fn contexts() -> &'static Mutex<std::collections::HashMap<usize, ContextInfo>> {
    static CONTEXTS: OnceLock<Mutex<std::collections::HashMap<usize, ContextInfo>>> = OnceLock::new();
    CONTEXTS.get_or_init(Default::default)
}
fn next_serial() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
/// Record a context created here; `share` joins its group.
fn register_context(context: usize, share: usize) {
    if context == 0 { return; }
    let mut map = contexts().lock().unwrap_or_else(|e| e.into_inner());
    let serial = next_serial();
    let group = if share == 0 { serial } else {
        map.get(&share).map(|info| info.group).unwrap_or_else(next_serial)
    };
    map.insert(context, ContextInfo { serial, group });
}
fn forget_context(context: usize) {
    contexts().lock().unwrap_or_else(|e| e.into_inner()).remove(&context);
}
/// A context created elsewhere (straight through the host EGL) is its own group.
fn context_info(context: usize) -> ContextInfo {
    *contexts().lock().unwrap_or_else(|e| e.into_inner()).entry(context).or_insert_with(|| {
        let serial = next_serial();
        ContextInfo { serial, group: serial }
    })
}

/// Substitute the cached host display for `EGL_NO_DISPLAY`.
///
/// hwui's `EglManager` keeps its own `mEglDisplay`, but a few call sites — the
/// `SkiaOpenGLPipeline` cleanup paths in particular — pass `EGL_NO_DISPLAY` or
/// what `eglGetCurrentDisplay` returned after a context teardown. Forwarding
/// that to the host EGL produces `EGL_BAD_DISPLAY` from Mesa's
/// `validate_display`, which makes every subsequent swap fail.
fn resolve_display(dpy: EGLDisplay) -> EGLDisplay {
    if dpy.is_null() {
        let cached = lock_state().display;
        cached as EGLDisplay
    } else {
        dpy
    }
}

fn env_dim(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

/// Lazily create the colour texture + FBO for `surface`. The host context must
/// be current on this thread.
/// The texture and FBO the *current* context renders this surface through,
/// created on first use: the texture once per share group, the FBO once per
/// context.
unsafe fn ensure_fbo(surface: EGLSurface, host: &Host) -> Result<(u32, u32), String> {
    let rec = &mut *(surface as *mut SurfaceRec);
    let current = host.get_current_context.map(|f| f() as usize).filter(|c| *c != 0)
        .unwrap_or_else(|| CURRENT_CONTEXT.with(|c| c.get()));
    if current == 0 {
        return Err("no current context for the surface's framebuffer".into());
    }
    let info = context_info(current);
    if let Some(target) = rec.targets.iter().find(|t| t.group == info.group) {
        if let Some(&(_, fbo)) = target.fbos.iter().find(|(serial, _)| *serial == info.serial) {
            return Ok((target.tex, fbo));
        }
    }

    let gen_textures = host
        .gl_gen_textures
        .ok_or_else(|| "host glGenTextures is unavailable".to_string())?;
    let bind_texture = host
        .gl_bind_texture
        .ok_or_else(|| "host glBindTexture is unavailable".to_string())?;
    let tex_image_2d = host
        .gl_tex_image_2d
        .ok_or_else(|| "host glTexImage2D is unavailable".to_string())?;
    let tex_parameter_i = host
        .gl_tex_parameter_i
        .ok_or_else(|| "host glTexParameteri is unavailable".to_string())?;
    let gen_framebuffers = host
        .gl_gen_framebuffers
        .ok_or_else(|| "host glGenFramebuffers is unavailable".to_string())?;
    let bind_framebuffer = host
        .gl_bind_framebuffer
        .ok_or_else(|| "host glBindFramebuffer is unavailable".to_string())?;
    let framebuffer_texture_2d = host
        .gl_framebuffer_texture_2d
        .ok_or_else(|| "host glFramebufferTexture2D is unavailable".to_string())?;

    let mut gralloc = None;
    let (tex, depth_rbo) = match rec.targets.iter().find(|t| t.group == info.group) {
        Some(target) => (target.tex, target.depth_rbo),
        None => {
            // Creating the texture must not disturb the context's own
            // GL_TEXTURE_2D binding.
            let previous = host_get_integer(host, 0x8069) as u32; // GL_TEXTURE_BINDING_2D
            let mut tex = 0u32;
            gen_textures(1, &mut tex);
            if tex == 0 {
                return Err("glGenTextures returned 0".into());
            }
            bind_texture(GL_TEXTURE_2D, tex);
            let mut plain_texture = true;
            let gralloc_requested = std::env::var_os("PENGUDROID_GRALLOC").is_some()
                && std::env::var_os("PENGUDROID_FORCE_DMABUF").is_none();
            if gralloc_requested {
                let attached = gralloc::support(host).and_then(|()| unsafe {
                    gralloc::attach_gralloc_texture(host, tex, rec.w, rec.h)
                });
                match attached {
                    Ok(target) => {
                        eprintln!(
                            "[gleshim:gralloc] rendering {}x{} through fd={} stride={}",
                            rec.w, rec.h, target.fd, target.stride,
                        );
                        gralloc = Some(target);
                        plain_texture = false;
                    }
                    Err(error) => {
                        eprintln!("[gleshim:gralloc] unsupported or texture attach failed, using dma-buf-capable ordinary texture: {error}");
                    }
                }
            }
            if plain_texture {
                tex_image_2d(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA8,
                    rec.w as c_int,
                    rec.h as c_int,
                    0,
                    GL_RGBA as u32,
                    GL_UNSIGNED_BYTE,
                    std::ptr::null(),
                );
            }
            tex_parameter_i(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            tex_parameter_i(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            bind_texture(GL_TEXTURE_2D, previous);

            let mut depth_rbo = 0u32;
            if let (
                Some(gen_rb),
                Some(bind_rb),
                Some(rb_storage),
            ) = (
                host.gl_gen_renderbuffers,
                host.gl_bind_renderbuffer,
                host.gl_renderbuffer_storage,
            ) {
                let prev_rb = host_get_integer(host, 0x8CA7) as u32; // GL_RENDERBUFFER_BINDING
                gen_rb(1, &mut depth_rbo);
                if depth_rbo != 0 {
                    bind_rb(GL_RENDERBUFFER, depth_rbo);
                    rb_storage(
                        GL_RENDERBUFFER,
                        GL_DEPTH24_STENCIL8,
                        rec.w as c_int,
                        rec.h as c_int,
                    );
                }
                bind_rb(GL_RENDERBUFFER, prev_rb);
            }

            rec.targets.push(Target { group: info.group, tex, depth_rbo, gralloc, flip: None, fbos: Vec::new() });
            (tex, depth_rbo)
        }
    };

    let mut fbo = 0u32;
    gen_framebuffers(1, &mut fbo);
    if fbo == 0 {
        return Err("glGenFramebuffers returned 0".into());
    }
    bind_framebuffer(GL_FRAMEBUFFER, fbo);
    framebuffer_texture_2d(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    if depth_rbo != 0 {
        if let Some(fb_rb) = host.gl_framebuffer_renderbuffer {
            fb_rb(
                GL_FRAMEBUFFER,
                GL_DEPTH_STENCIL_ATTACHMENT,
                GL_RENDERBUFFER,
                depth_rbo,
            );
            let complete = host
                .gl_check_framebuffer_status
                .map_or(true, |check| check(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
            if !complete {
                fb_rb(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, 0);
                if let (Some(bind_rb), Some(rb_storage)) =
                    (host.gl_bind_renderbuffer, host.gl_renderbuffer_storage)
                {
                    let prev_rb = host_get_integer(host, 0x8CA7) as u32;
                    bind_rb(GL_RENDERBUFFER, depth_rbo);
                    rb_storage(
                        GL_RENDERBUFFER,
                        GL_DEPTH_COMPONENT16,
                        rec.w as c_int,
                        rec.h as c_int,
                    );
                    bind_rb(GL_RENDERBUFFER, prev_rb);
                    fb_rb(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, depth_rbo);
                    let ok16 = host
                        .gl_check_framebuffer_status
                        .map_or(true, |check| check(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
                    if !ok16 {
                        fb_rb(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, 0);
                    }
                }
            }
        }
    }
    let target = rec.targets.iter_mut().find(|t| t.group == info.group).unwrap();
    target.fbos.push((info.serial, fbo));
    // The daemon's EGL presenter samples the guest texture directly,
    // so the guest renders its gralloc allocation with no extra flip pass.
    // The manual android_wlegl buffer transport still needs the flip copies.
    let direct_wlegl = gralloc::wlegl_requested()
        && !gralloc::async_requested()
        && !gralloc::wlegl_buffer_requested();
    if target.gralloc.is_some() && target.flip.is_none()
        && !direct_wlegl
        && std::env::var_os("PENGUDROID_GRALLOC_SHM").is_some()
    {
        match gralloc::create_gralloc_flip(host, rec.w, rec.h) {
            Ok(flip) => {
                eprintln!(
                    "[gleshim:gralloc] GPU Y-flip enabled through fd={} fbo={}",
                    flip.buffers[0].target.fd, flip.buffers[0].fbo,
                );
                target.flip = Some(flip);
            }
            Err(error) => eprintln!("[gleshim:gralloc] GPU Y-flip unavailable: {error}"),
        }
        if gralloc::async_requested() {
            let flip = target.flip.as_mut().unwrap();
            if flip.buffers.len() == 1 {
                match gralloc::create_gralloc_flip_buffer(host, rec.w, rec.h) {
                    Ok(buffer) => {
                        eprintln!(
                            "[gleshim:gralloc] async double buffering enabled through second fd={} fbo={}",
                            buffer.target.fd, buffer.fbo,
                        );
                        flip.buffers.push(buffer);
                    }
                    Err(error) => eprintln!("[gleshim:gralloc] second flip buffer unavailable: {error}"),
                }
            }
        }
        if let Some(bind) = host.gl_bind_framebuffer {
            bind(GL_FRAMEBUFFER, fbo);
        }
    }
    Ok((tex, fbo))
}

/// Bind the surface's FBO and set the viewport, creating it first if needed.
unsafe fn bind_surface_fbo(surface: EGLSurface, host: &Host) -> Result<(), String> {
    let (_, fbo) = ensure_fbo(surface, host)?;
    let rec = &*(surface as *const SurfaceRec);
    if let Some(bind_framebuffer) = host.gl_bind_framebuffer {
        bind_framebuffer(GL_FRAMEBUFFER, fbo);
    }
    if let Some(viewport) = host.gl_viewport {
        viewport(0, 0, rec.w as c_int, rec.h as c_int);
    }
    Ok(())
}

/// `glGetIntegerv(pname)` on the current host context (0 if unavailable).
unsafe fn host_get_integer(host: &Host, pname: u32) -> i32 {
    type GetIntegerv = unsafe extern "C" fn(u32, *mut i32);
    static REAL: OnceLock<Option<GetIntegerv>> = OnceLock::new();
    let real = REAL.get_or_init(|| {
        let gpa = host.get_proc_address?;
        let p = gpa(c"glGetIntegerv".as_ptr());
        (!p.is_null()).then(|| std::mem::transmute::<*mut c_void, GetIntegerv>(p))
    });
    let mut value = 0;
    if let Some(get) = real {
        get(pname, &mut value);
    }
    value
}

/// Whether framebuffer `name` is what the context sees as its default
/// framebuffer: 0 itself, or one of the surface FBOs this shim made for the
/// context (`ensure_fbo`), which stand in for 0.
fn is_default_framebuffer(name: u32, serial: u64) -> bool {
    if name == 0 {
        return true;
    }
    let surfaces: Vec<usize> = live_surfaces().lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect();
    surfaces.into_iter().any(|surface| {
        // SAFETY: live surfaces are SurfaceRecs owned by this shim.
        let rec = unsafe { &*(surface as *const SurfaceRec) };
        rec.targets.iter().any(|t| t.fbos.iter().any(|&(s, fbo)| s == serial && fbo == name))
    })
}

/// `eglMakeCurrent`'s half of default-framebuffer emulation.
///
/// Real EGL never changes a context's framebuffer bindings: making a context
/// current only re-targets *framebuffer 0* at the new surface, and sets the
/// viewport the first time the context is made current. Binding the surface
/// FBO unconditionally overwrote the app's own FBO binding on every switch.
/// Chromium tracks its bindings and does not rebind after a switch, so a
/// WebGL clear landed in this shim's FBO instead of the canvas, and the
/// canvas reached the screen blank.
unsafe fn make_current_surface_fbo(surface: EGLSurface, host: &Host) -> Result<(), String> {
    const GL_DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
    const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
    const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
    const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
    let current = CURRENT_CONTEXT.with(|c| c.get());
    let info = context_info(current);
    let first_time = {
        static SEEN: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let first = !seen.contains(&info.serial);
        if first {
            seen.push(info.serial);
        }
        first
    };
    let draw = host_get_integer(host, GL_DRAW_FRAMEBUFFER_BINDING) as u32;
    let read = host_get_integer(host, GL_READ_FRAMEBUFFER_BINDING) as u32;
    let draw_default = first_time || is_default_framebuffer(draw, info.serial);
    let read_default = first_time || is_default_framebuffer(read, info.serial);
    let (_, fbo) = ensure_fbo(surface, host)?;
    if let Some(bind_framebuffer) = host.gl_bind_framebuffer {
        match (draw_default, read_default) {
            (true, true) => bind_framebuffer(GL_FRAMEBUFFER, fbo),
            (true, false) => {
                bind_framebuffer(GL_DRAW_FRAMEBUFFER, fbo);
                bind_framebuffer(GL_READ_FRAMEBUFFER, read);
            }
            (false, true) => {
                bind_framebuffer(GL_DRAW_FRAMEBUFFER, draw);
                bind_framebuffer(GL_READ_FRAMEBUFFER, fbo);
            }
            // `ensure_fbo` may have bound a new FBO while creating it.
            (false, false) => {
                bind_framebuffer(GL_DRAW_FRAMEBUFFER, draw);
                bind_framebuffer(GL_READ_FRAMEBUFFER, read);
            }
        }
    }
    if first_time {
        let rec = &*(surface as *const SurfaceRec);
        if let Some(viewport) = host.gl_viewport {
            viewport(0, 0, rec.w as c_int, rec.h as c_int);
        }
    }
    Ok(())
}

/// Make sure a host context is current on this thread before any GL/export.
unsafe fn ensure_host_context(host: &Host) -> Result<(), String> {
    if let Some(get_current_context) = host.get_current_context {
        if !get_current_context().is_null() {
            return Ok(());
        }
        let (display, fallback) = {
            let st = lock_state();
            (st.display, st.context)
        };
        let own = CURRENT_CONTEXT.with(|c| c.get());
        let context = if own != 0 { own } else { fallback };
        if display != 0 && context != 0 {
            let ok = (host.make_current)(
                display as EGLDisplay,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                context as EGLContext,
            );
            if ok == EGL_TRUE {
                return Ok(());
            }
        }
        Err("no current EGL context".into())
    } else {
        // We cannot query the driver; assume the app made the context current.
        Ok(())
    }
}

fn host_error(host: &Host) -> String {
    // SAFETY: plain entry point with no arguments.
    let code = unsafe { (host.get_error)() };
    format!("EGL error 0x{code:04X}")
}

// ---------------------------------------------------------------------------
// Daemon presentation
// ---------------------------------------------------------------------------

fn close_fds(fds: &[RawFd]) {
    for &fd in fds {
        if fd >= 0 {
            // SAFETY: the socket layer transferred ownership of these fds.
            unsafe { libc::close(fd) };
        }
    }
}

/// Connect + handshake + resolve/create the surface id, lazily and once.
fn ensure_surface_id(st: &mut State, rec: &mut SurfaceRec) -> Result<u32, String> {
    // An early id means attach, not "no connection needed": PresentShm still
    // needs the request/reply channel established below.
    let subscribed_at_create = rec.surface_id != 0;

    if st.conn.is_none() {
        if st.connect_attempted {
            return Err("daemon is unavailable".into());
        }
        st.connect_attempted = true;

        let conn = Connection::connect_endpoint(&default_socket_path())
            .map_err(|e| format!("cannot connect to the daemon: {e}"))?;
        // Cold daemon start (fontconfig / Adwaita initialization) can take ~1s on
        // first surface creation; give it a 5s budget and allow recovery on timeout.
        let _ = conn.set_read_timeout(Some(Duration::from_millis(5000)));

        let hello = Message::Hello {
            version: PROTO_VERSION,
            role: Role::Guest,
            pid: std::process::id() as i32,
        };
        if let Err(e) = conn.send_message(&hello, &[]) {
            st.connect_attempted = false;
            return Err(format!("Hello failed: {e}"));
        }
        let (reply, fds) = match conn.recv_message() {
            Ok(v) => v,
            Err(e) => {
                st.connect_attempted = false;
                return Err(format!("Welcome failed: {e}"));
            }
        };
        close_fds(&fds);
        match reply {
            Message::Welcome { .. } => {}
            other => {
                st.connect_attempted = false;
                return Err(format!("unexpected handshake reply: {other:?}"));
            }
        }
        st.conn = Some(conn);
    }

    if env_truthy("PENGUDROID_ANDROID_LAYERS") && !rec.layer_resolved && rec.native_window != 0 {
        rec.layer_resolved = true;
        let native_window = rec.native_window as u64;
        let query = |native_window| -> Result<Message, String> {
            let conn = st.conn.as_ref().ok_or("daemon is unavailable")?;
            conn.send_message(&Message::ResolveWindowLayer { native_window }, &[])
                .map_err(|e| e.to_string())?;
            let (reply, fds) = conn.recv_message().map_err(|e| e.to_string())?;
            close_fds(&fds);
            Ok(reply)
        };
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        let mut result;
        loop {
            result = query(native_window);
            // Android 17's Surface.mNativeObject addresses the containing native
            // Surface; HWUI passes the ANativeWindow base 16 bytes into it. This
            // is only a metadata-key fallback, never a pointer dereference.
            if native_window >= 16 && matches!(&result,
                Ok(Message::WindowLayer { window_type: 0, .. }))
            {
                result = query(native_window - 16);
            }
            let pending = matches!(&result, Ok(Message::WindowLayer { window_type: 0, .. }));
            if !pending || std::time::Instant::now() >= deadline { break; }
            // The main Looper remains free to finish traversal and publish the
            // real ViewRoot metadata. Never spin a free-running frame producer.
            std::thread::sleep(Duration::from_millis(5));
        }
        match result {
            Ok(Message::WindowLayer { window_type, visible, root_id, .. }) => {
                eprintln!("[gleshim] Android layer native={native_window:#x} root={root_id} type={window_type} visible={visible}");
                if window_type > 1 && visible {
                    rec.child_of_early = true;
                    rec.surface_id = 0;
                }
            }
            Ok(other) => return Err(format!("unexpected WindowLayer reply: {other:?}")),
            Err(error) => {
                st.conn = None;
                st.connect_attempted = false;
                return Err(format!("ResolveWindowLayer failed: {error}"));
            }
        }
    }

    if rec.surface_id != 0 {
        let id = rec.surface_id;
        {
            rec.surface_id = id;
            // The launcher may have created an early loading surface. Attach
            // input and preferences just as for a guest-created surface.
            if let Some(conn) = st.conn.as_ref() {
                let fps_meter = mangohud_enabled();
                conn.send_message(&Message::SetSurfaceOptions { surface_id: id, fps_meter }, &[])
                    .map_err(|e| format!("surface preferences: {e}"))?;
                let (reply, fds) = conn.recv_message().map_err(|e| e.to_string())?;
                close_fds(&fds);
                if let Message::Error { message, .. } = reply { return Err(message); }
            }
            if !subscribed_at_create {
                eprintln!("[pengudroid-gleshim] attaching input to early surface {id}");
                subscribe_input(id);
            }
            return Ok(id);
        }
    }

    let (w, h) = (rec.w, rec.h);
    let request = if rec.child_of_early {
        Message::CreateSubsurface {
            parent_pid: std::process::id() as i32,
            x: 0,
            y: 0,
            width: w,
            height: h,
            native_window: rec.native_window as u64,
        }
    } else {
        Message::CreateSurface {
            width: w,
            height: h,
            title: std::env::var("PENGUDROID_APP_LABEL").unwrap_or_else(|_| "PenguDroid".into()),
        }
    };
    let send_result = match st.conn.as_ref() {
        Some(conn) => conn.send_message(&request, &[]),
        None => return Err("daemon is unavailable".into()),
    };
    if let Err(e) = send_result {
        st.conn = None;
        st.connect_attempted = false;
        return Err(format!("CreateSurface failed: {e}"));
    }

    let recv_result = match st.conn.as_ref() {
        Some(conn) => conn.recv_message(),
        None => return Err("daemon is unavailable".into()),
    };
    let (reply, fds) = match recv_result {
        Ok(v) => v,
        Err(e) => {
            st.conn = None;
            st.connect_attempted = false;
            return Err(format!("SurfaceCreated failed: {e}"));
        }
    };
    close_fds(&fds);
    match reply {
        Message::SurfaceCreated { surface_id, .. }
        | Message::SubsurfaceCreated { surface_id } => {
            rec.surface_id = surface_id;
            rec.owns_surface = true;
            // The host's resize follower lives in this same process and has to
            // know which surface the window manager is scaling. It gets its own
            // variable: `PENGUDROID_SURFACE_ID` is an *input* that makes a later
            // window attach to an existing surface instead of creating one, so
            // writing it here would alias a game's second window onto its first.
            // The newest window wins because that is the one that stays: a game
            // creates a splash surface and destroys it when the real one appears.
            if !rec.child_of_early {
                std::env::set_var("PENGUDROID_LAST_SURFACE_ID", surface_id.to_string());
            }
            if let Some(conn) = st.conn.as_ref() {
                let fps_meter = mangohud_enabled();
                if let Err(e) = conn.send_message(&Message::SetSurfaceOptions { surface_id, fps_meter }, &[]) {
                    st.conn = None;
                    st.connect_attempted = false;
                    return Err(format!("surface preferences: {e}"));
                }
                let (reply, fds) = match conn.recv_message() {
                    Ok(v) => v,
                    Err(e) => {
                        st.conn = None;
                        st.connect_attempted = false;
                        return Err(e.to_string());
                    }
                };
                close_fds(&fds);
                if let Message::Error { message, .. } = reply { return Err(message); }
            }
            if !rec.child_of_early { subscribe_input(surface_id); }
            Ok(surface_id)
        }
        Message::Error { code, message } => Err(format!(
            "CreateSurface rejected ({code}): {message}"
        )),
        other => Err(format!("unexpected CreateSurface reply: {other:?}")),
    }
}

/// Input events forwarded by the daemon, waiting for the host to pick them up.
static INPUT_QUEUE: Mutex<std::collections::VecDeque<(u32, pengudroid_proto::InputKind)>> =
    Mutex::new(std::collections::VecDeque::new());
static INPUT_AVAILABLE: Condvar = Condvar::new();

#[derive(Clone, Copy)]
struct TouchDebugState {
    x: f32,
    y: f32,
    pressed: bool,
    last_ms: u64,
}

static TOUCH_DEBUG_STATE: Mutex<TouchDebugState> = Mutex::new(TouchDebugState {
    x: 0.0,
    y: 0.0,
    pressed: false,
    last_ms: 0,
});

fn input_trace_now_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64).saturating_mul(1000) + ts.tv_nsec as u64 / 1_000_000
}

struct FrameTraceState {
    frame: u64,
    last_swap_ms: u64,
    touch_seq: u64,
    touch_kind: &'static str,
    touch_recv_ms: u64,
    swapped_seq: u64,
    swapped_kind: &'static str,
    swapped_recv_ms: u64,
    presented_seq: u64,
}

static FRAME_TRACE_STATE: Mutex<FrameTraceState> = Mutex::new(FrameTraceState {
    frame: 0,
    last_swap_ms: 0,
    touch_seq: 0,
    touch_kind: "none",
    touch_recv_ms: 0,
    swapped_seq: 0,
    swapped_kind: "none",
    swapped_recv_ms: 0,
    presented_seq: 0,
});

static CONTENT_HASHES: std::sync::LazyLock<Mutex<std::collections::HashMap<usize, u64>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Hash a narrow strip containing country names. Unlike eglSwapBuffers alone,
/// this distinguishes a real visual scroll update from resubmitting unchanged
/// pixels. Readback is deliberately opt-in because it can affect timing.
unsafe fn trace_content_hash(surface: EGLSurface, host: &Host) {
    if std::env::var_os("PENGUDROID_TRACE_CONTENT").is_none() {
        return;
    }
    let Some(read_pixels) = host.gl_read_pixels else { return };
    let rec = &*(surface as *const SurfaceRec);
    let w = 64.min(rec.w.max(1) as usize);
    let h = 512.min(rec.h.max(1) as usize);
    let x = 120u32.min(rec.w.saturating_sub(w as u32));
    let top = 350u32.min(rec.h.saturating_sub(h as u32));
    let y = rec.h - top - h as u32;
    let mut pixels = vec![0u8; w * h * 4];
    read_pixels(
        x as i32, y as i32, w as i32, h as i32, GL_RGBA as u32,
        GL_UNSIGNED_BYTE, pixels.as_mut_ptr().cast(),
    );
    let mut hash = 0xcbf29ce484222325u64;
    for byte in pixels {
        hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
    }
    let now = input_trace_now_ms();
    let mut hashes = CONTENT_HASHES.lock().unwrap_or_else(|e| e.into_inner());
    if hashes.get(&(surface as usize)) == Some(&hash) {
        return;
    }
    hashes.insert(surface as usize, hash);
    let state = FRAME_TRACE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    eprintln!(
        "[gleshim:content] ts={now} frame={} surface={:x} hash={hash:016x} last_touch_seq={} kind={} since_touch_ms={}",
        state.frame, surface as usize, state.touch_seq, state.touch_kind,
        now.saturating_sub(state.touch_recv_ms),
    );
}

fn record_frame_touch(event: &pengudroid_proto::InputKind) {
    if matches!(event, pengudroid_proto::InputKind::PointerButton { pressed: true, .. }) {
        notify_input_wakeup_vsync();
    }
    if std::env::var_os("PENGUDROID_TRACE_INPUT_TIMING").is_none() {
        return;
    }
    let kind = match event {
        pengudroid_proto::InputKind::PointerMotion { .. } => "move",
        pengudroid_proto::InputKind::PointerButton { pressed: true, .. } => "down",
        pengudroid_proto::InputKind::PointerButton { pressed: false, .. } => "up",
        _ => return,
    };
    let mut state = FRAME_TRACE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.touch_seq = state.touch_seq.wrapping_add(1);
    state.touch_kind = kind;
    state.touch_recv_ms = input_trace_now_ms();
}

fn trace_touch_swap() {
    if std::env::var_os("PENGUDROID_TRACE_INPUT_TIMING").is_none() {
        return;
    }
    let now = input_trace_now_ms();
    let mut state = FRAME_TRACE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.frame = state.frame.wrapping_add(1);
    let gap = if state.last_swap_ms == 0 { 0 } else { now.saturating_sub(state.last_swap_ms) };
    state.last_swap_ms = now;
    if state.touch_seq > state.swapped_seq {
        state.swapped_seq = state.touch_seq;
        state.swapped_kind = state.touch_kind;
        state.swapped_recv_ms = state.touch_recv_ms;
        eprintln!(
            "[gleshim:frame] ts={now} frame={} gap_ms={gap} touch_seq={} kind={} touch_to_swap_ms={}",
            state.frame, state.touch_seq, state.touch_kind,
            now.saturating_sub(state.touch_recv_ms),
        );
    } else {
        eprintln!("[gleshim:frame] ts={now} frame={} gap_ms={gap} touch=none", state.frame);
    }
}

fn trace_touch_present(
    path: &str,
    success: bool,
    finish: Duration,
    flip: Duration,
    present: Duration,
) {
    if std::env::var_os("PENGUDROID_TRACE_INPUT_TIMING").is_none() {
        return;
    }
    let now = input_trace_now_ms();
    let mut state = FRAME_TRACE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.presented_seq >= state.swapped_seq {
        return;
    }
    // A newer touch which arrived during this frame belongs to the next swap.
    let seq = state.swapped_seq;
    state.presented_seq = seq;
    eprintln!(
        "[gleshim:frame] ts={now} frame={} path={path} success={success} touch_seq={seq} kind={} touch_to_present_ms={} finish_us={} flip_us={} present_us={}",
        state.frame, state.swapped_kind, now.saturating_sub(state.swapped_recv_ms),
        finish.as_micros(), flip.as_micros(), present.as_micros(),
    );
}

fn notify_frame_presented_vsync() {
    if std::env::var_os("PENGUDROID_VSYNC_ON_PRESENT").is_none() {
        return;
    }
    static NOTIFY: OnceLock<usize> = OnceLock::new();
    let address = *NOTIFY.get_or_init(|| unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            b"__pengudroid_vsync_frame_presented\0".as_ptr().cast(),
        ) as usize
    });
    if address != 0 {
        let notify: unsafe extern "C" fn() = unsafe { std::mem::transmute(address) };
        unsafe { notify() };
    }
}

fn notify_input_wakeup_vsync() {
    if std::env::var_os("PENGUDROID_VSYNC_ON_PRESENT").is_none() {
        return;
    }
    static NOTIFY: OnceLock<usize> = OnceLock::new();
    let address = *NOTIFY.get_or_init(|| unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            b"__pengudroid_vsync_input_wakeup\0".as_ptr().cast(),
        ) as usize
    });
    if address != 0 {
        let notify: unsafe extern "C" fn() = unsafe { std::mem::transmute(address) };
        unsafe { notify() };
    }
}

fn record_touch_debug(event: &pengudroid_proto::InputKind) {
    if std::env::var_os("PENGUDROID_TOUCH_DEBUG").is_none() {
        return;
    }
    let mut state = TOUCH_DEBUG_STATE.lock().unwrap_or_else(|e| e.into_inner());
    match event {
        pengudroid_proto::InputKind::PointerEnter { x, y }
        | pengudroid_proto::InputKind::PointerMotion { x, y } => {
            state.x = *x;
            state.y = *y;
            state.last_ms = input_trace_now_ms();
        }
        pengudroid_proto::InputKind::PointerButton { pressed, .. } => {
            state.pressed = *pressed;
            state.last_ms = input_trace_now_ms();
        }
        pengudroid_proto::InputKind::PointerLeave => {
            state.pressed = false;
            state.last_ms = input_trace_now_ms();
        }
        _ => {}
    }
}

/// Text is kept out of the fixed 16-byte pointer-event record. The input pump
/// immediately follows tag 8 with pengudroid_input_read_text.
static TEXT_INPUT_QUEUE: Mutex<std::collections::VecDeque<String>> =
    Mutex::new(std::collections::VecDeque::new());

/// Queue a key as if the compositor had sent it (test hook for
/// `PENGUDROID_SYNTH_KEYS`): it then takes exactly the path a real key does.
#[no_mangle]
pub extern "C" fn pengudroid_input_inject_key(keycode: u32, pressed: i32) {
    INPUT_QUEUE.lock().unwrap_or_else(|e| e.into_inner())
        .push_back((0, pengudroid_proto::InputKind::Key { keycode, pressed: pressed != 0 }));
    INPUT_AVAILABLE.notify_one();
}

/// Subscribe to the compositor's input for `surface_id` on a second connection.
///
/// The present connection is strictly request/reply, so unsolicited input would
/// desynchronise it; a dedicated connection keeps the two streams apart. The
/// reader thread only ever appends to [`INPUT_QUEUE`].
fn subscribe_input(surface_id: u32) {
    HAS_WINDOW.store(true, std::sync::atomic::Ordering::Relaxed);
    // Start the compositor-clock subscription as soon as a real window exists,
    // independently of the first requestNextVsync (which some apps never
    // issue, e.g. MAX).
    if std::env::var_os("PENGUDROID_VSYNC_COMPOSITOR").is_some() {
        let ptr = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                b"__pengudroid_vsync_request\0".as_ptr().cast(),
            )
        };
        if ptr.is_null() {
            eprintln!("[gleshim] compositor vsync: shim hook unavailable");
        } else {
            eprintln!("[gleshim] compositor vsync: starting via shim hook (surface {surface_id})");
            let hook: unsafe extern "C" fn(usize) = unsafe { std::mem::transmute(ptr) };
            unsafe { hook(0) };
            eprintln!("[gleshim] compositor vsync: shim hook returned");
        }
    }
    std::thread::spawn(move || {
        let conn = match Connection::connect_endpoint(&default_socket_path()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[pengudroid-gleshim] input: cannot connect: {e}");
                return;
            }
        };
        let hello = Message::Hello {
            version: PROTO_VERSION,
            role: Role::Guest,
            pid: std::process::id() as i32,
        };
        if conn.send_message(&hello, &[]).is_err() {
            return;
        }
        if conn.recv_message().is_err() {
            return;
        }
        if let Err(error) = conn.send_message(&Message::SubscribeInput { surface_id }, &[]) {
            eprintln!("[pengudroid-gleshim] input: subscribe send failed: {error}");
            return;
        }
        if let Err(error) = conn.recv_message() {
            eprintln!("[pengudroid-gleshim] input: subscribe reply failed: {error}");
            return;
        }
        eprintln!("[pengudroid-gleshim] input: subscribed to surface {surface_id}");
        loop {
            match conn.recv_message() {
                Ok((Message::InputEvent { surface_id, event }, fds)) => {
                    close_fds(&fds);
                    if let pengudroid_proto::InputKind::Vsync { vsync_time_ns, interval_ns } = event {
                        // A compositor frame pulse is the display clock, not
                        // user input: deliver it straight to the shim's clock
                        // instead of queueing it as a pointer/key event.
                        let ptr = unsafe {
                            libc::dlsym(
                                libc::RTLD_DEFAULT,
                                b"__pengudroid_vsync_compositor_pulse\0".as_ptr().cast(),
                            )
                        };
                        if !ptr.is_null() {
                            let hook: unsafe extern "C" fn(i64, i64) =
                                unsafe { std::mem::transmute(ptr) };
                            unsafe { hook(vsync_time_ns, interval_ns) };
                        }
                        continue;
                    }
                    record_touch_debug(&event);
                    record_frame_touch(&event);
                    if std::env::var_os("PENGUDROID_TRACE_INPUT_TIMING").is_some() {
                        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
                        let ms = ts.tv_sec as u128 * 1000 + ts.tv_nsec as u128 / 1_000_000;
                        eprintln!("[gleshim:input] ts={ms} recv {event:?}");
                    }
                    if std::env::var_os("PENGUDROID_TRACE_INPUT").is_some() {
                        match &event {
                            pengudroid_proto::InputKind::CommitText { text }
                            | pengudroid_proto::InputKind::PreeditText { text } => eprintln!(
                                "[gleshim:input] text event ({} bytes) for surface {surface_id}",
                                text.len(),
                            ),
                            _ => eprintln!("[gleshim:input] received {event:?} for surface {surface_id}"),
                        }
                    }
                    let mut q = INPUT_QUEUE.lock().unwrap();
                    if q.len() < 256 {
                        // Wayland pointer motion is coalesced only when it is
                        // the newest queued event; enter/button/up boundaries
                        // remain ordered. Hardware evdev is coalesced in its
                        // own SYN_REPORT drain before Android dispatch.
                        if let Some((last_surface, last_event)) = q.back_mut() {
                            if *last_surface == surface_id
                                && matches!(last_event, pengudroid_proto::InputKind::PointerMotion { .. })
                                && matches!(&event, pengudroid_proto::InputKind::PointerMotion { .. })
                            {
                                *last_event = event;
                            } else {
                                q.push_back((surface_id, event));
                            }
                        } else {
                            q.push_back((surface_id, event));
                        }
                        drop(q);
                        INPUT_AVAILABLE.notify_one();
                    }
                }
                Ok((other, fds)) => {
                    close_fds(&fds);
                    eprintln!("[gleshim:input] unexpected {:?} on surface {surface_id}", other.kind());
                }
                Err(e) => {
                    eprintln!("[gleshim:input] surface {surface_id} disconnected: {e}");
                    return;
                }
            }
        }
    });
}

/// Tell the daemon that Android's focused View accepts text. Called only on
/// actual focus changes, never per redraw. Returns 1 after daemon acceptance.
#[no_mangle]
pub extern "C" fn pengudroid_set_ime_state(visible: i32) -> i32 {
    let surface_id = ["PENGUDROID_LAST_SURFACE_ID", "PENGUDROID_SURFACE_ID"]
        .iter().find_map(|name| std::env::var(name).ok()?.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if surface_id == 0 { return 0; }
    let result = (|| -> Result<bool, String> {
        let conn = Connection::connect_endpoint(&default_socket_path()).map_err(|e| e.to_string())?;
        conn.send_message(&Message::Hello {
            version: PROTO_VERSION,
            role: Role::Guest,
            pid: std::process::id() as i32,
        }, &[]).map_err(|e| e.to_string())?;
        let (_, fds) = conn.recv_message().map_err(|e| e.to_string())?;
        close_fds(&fds);
        conn.send_message(&Message::SetImeState {
            surface_id,
            visible: visible != 0,
        }, &[]).map_err(|e| e.to_string())?;
        let (reply, fds) = conn.recv_message().map_err(|e| e.to_string())?;
        close_fds(&fds);
        match reply {
            Message::Ack => Ok(true),
            Message::Error { message, .. } => Err(message),
            other => Err(format!("unexpected IME reply: {other:?}")),
        }
    })();
    match result {
        Ok(value) => i32::from(value),
        Err(error) => {
            eprintln!("[gleshim:ime] {error}");
            0
        }
    }
}

/// Whether any window exists yet; before that there is nothing to lock to.
static HAS_WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lock (`locked != 0`) or release the pointer on the app's window.
///
/// While locked the compositor hides the cursor and motion arrives through
/// [`pengudroid_input_poll`] as kind 7 (`a` = dx, `b` = dy). Returns 1 when the
/// daemon accepted the request, 0 otherwise — no window yet, no daemon, or a
/// compositor without `pointer-constraints`/`relative-pointer`.
///
/// It uses a connection of its own: the present connection is strictly
/// request/reply and may be mid-swap on another thread.
#[no_mangle]
pub extern "C" fn pengudroid_pointer_lock(locked: i32) -> i32 {
    if !HAS_WINDOW.load(std::sync::atomic::Ordering::Relaxed) {
        return 0;
    }
    // 0: whichever of this process's surfaces has the focus.
    let surface_id = 0;
    let request = || -> Result<bool, String> {
        let conn = Connection::connect_endpoint(&default_socket_path()).map_err(|e| e.to_string())?;
        let hello = Message::Hello {
            version: PROTO_VERSION,
            role: Role::Guest,
            pid: std::process::id() as i32,
        };
        conn.send_message(&hello, &[]).map_err(|e| e.to_string())?;
        let (_, fds) = conn.recv_message().map_err(|e| e.to_string())?;
        close_fds(&fds);
        conn.send_message(&Message::SetPointerLock { surface_id, locked: locked != 0 }, &[])
            .map_err(|e| e.to_string())?;
        let (reply, fds) = conn.recv_message().map_err(|e| e.to_string())?;
        close_fds(&fds);
        match reply {
            Message::Ack => Ok(true),
            Message::Error { message, .. } => Err(message),
            other => Err(format!("unexpected {:?}", other.kind())),
        }
    };
    match request() {
        Ok(ok) => i32::from(ok),
        Err(e) => {
            eprintln!("[pengudroid-gleshim] pointer lock: {e}");
            0
        }
    }
}

/// Wake the input pump as soon as a reader thread queues an event. The timeout
/// still lets the same loop poll non-blocking hardware evdev and gamepads.
#[no_mangle]
pub extern "C" fn pengudroid_input_wait(timeout_ms: u32) {
    let queue = INPUT_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if queue.is_empty() {
        let _ = INPUT_AVAILABLE.wait_timeout(
            queue, Duration::from_millis(timeout_ms as u64),
        ).unwrap_or_else(|e| e.into_inner());
    }
}

/// Pop one queued input event into `out`.
///
/// Layout (all little-endian, 16 bytes): `[u32 surface_id][u32 kind]
/// [f32 a][f32 b]`, where `kind` is the protocol's `InputKind` tag and `a`/`b`
/// carry the coordinates (or the button/keycode and the pressed flag).
///
/// Returns 1 when an event was written, 0 when the queue is empty.
#[no_mangle]
pub unsafe extern "C" fn pengudroid_input_poll(out: *mut u8) -> i32 {
    if out.is_null() {
        return 0;
    }
    let event = {
        let mut q = INPUT_QUEUE.lock().unwrap();
        if q.is_empty() {
            return 0;
        }
        q.pop_front().unwrap()
    };
    let (surface_id, kind) = event;
    if std::env::var_os("PENGUDROID_TRACE_INPUT_TIMING").is_some() {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        let ms = ts.tv_sec as u128 * 1000 + ts.tv_nsec as u128 / 1_000_000;
        eprintln!("[gleshim:input] ts={ms} poll {kind:?}");
    }
    if std::env::var_os("PENGUDROID_TRACE_INPUT").is_some() {
        match &kind {
            pengudroid_proto::InputKind::CommitText { text }
            | pengudroid_proto::InputKind::PreeditText { text } => eprintln!(
                "[gleshim:input] polled text ({} bytes) for surface {surface_id}", text.len(),
            ),
            _ => eprintln!("[gleshim:input] polled {kind:?} for surface {surface_id}"),
        }
    }
    let (tag, a, b) = match kind {
        pengudroid_proto::InputKind::PointerEnter { x, y } => (1u32, x, y),
        pengudroid_proto::InputKind::PointerLeave => (2, 0.0, 0.0),
        pengudroid_proto::InputKind::PointerMotion { x, y } => (3, x, y),
        pengudroid_proto::InputKind::PointerButton { button, pressed } => {
            (4, button as f32, if pressed { 1.0 } else { 0.0 })
        }
        pengudroid_proto::InputKind::PointerAxis { axis, value } => (5, axis as f32, value),
        pengudroid_proto::InputKind::Key { keycode, pressed } => {
            (6, keycode as f32, if pressed { 1.0 } else { 0.0 })
        }
        pengudroid_proto::InputKind::PointerRelative { dx, dy } => (7, dx, dy),
        pengudroid_proto::InputKind::CommitText { text } => {
            let length = text.len() as f32;
            TEXT_INPUT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push_back(text);
            (8, length, 0.0)
        }
        pengudroid_proto::InputKind::PreeditText { text } => {
            let length = text.len() as f32;
            TEXT_INPUT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push_back(text);
            (8, length, 1.0)
        }
        pengudroid_proto::InputKind::FinishComposing => (10, 0.0, 0.0),
        pengudroid_proto::InputKind::ClipboardAction { action } => (11, action as f32, 0.0),
        pengudroid_proto::InputKind::WindowVisibility { visible } => {
            if visible { notify_input_wakeup_vsync(); }
            (12, if visible { 1.0 } else { 0.0 }, 0.0)
        }
        // Compositor frame pulses are consumed by the input subscription
        // thread itself; one that survives to here is simply not input.
        pengudroid_proto::InputKind::Vsync { .. } => (13, 0.0, 0.0),
    };
    let buf = std::slice::from_raw_parts_mut(out, 16);
    buf[0..4].copy_from_slice(&surface_id.to_le_bytes());
    buf[4..8].copy_from_slice(&tag.to_le_bytes());
    buf[8..12].copy_from_slice(&a.to_le_bytes());
    buf[12..16].copy_from_slice(&b.to_le_bytes());
    1
}

/// Copy the text associated with the just-polled tag 8. The buffer size is
/// exactly the byte length advertised in `a`; no NUL terminator is required.
#[no_mangle]
pub unsafe extern "C" fn pengudroid_input_read_text(out: *mut u8, capacity: u32) -> i32 {
    if out.is_null() { return -1; }
    let mut queue = TEXT_INPUT_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(text) = queue.pop_front() else { return -1 };
    if text.len() > capacity as usize {
        queue.push_front(text);
        return -1;
    }
    std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len());
    text.len() as i32
}

/// CPU presentation fallback for drivers without a working dma-buf export.
/// Enabled explicitly with `PENGUDROID_FORCE_SHM=1`; the default zero-copy
/// path is unchanged. GL rows are bottom-up, Wayland's shared frame is top-down.
unsafe fn present_shm(surface: EGLSurface) -> Result<(), String> {
    let profile = std::env::var_os("PENGUDROID_PROFILE_SHM").is_some();
    let started = std::time::Instant::now();
    let host = host()?;
    let read_pixels = host
        .gl_read_pixels
        .ok_or_else(|| "glReadPixels is unavailable for the SHM fallback".to_string())?;
    let rec = &mut *(surface as *mut SurfaceRec);
    let (w, h) = (rec.w as usize, rec.h as usize);
    let len = w
        .checked_mul(h)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| format!("SHM surface size overflows: {w}x{h}"))?;
    let mut rgba = vec![0u8; len];
    let read_format = if rec.targets.iter().any(|target| target.gralloc.is_some()) {
        0x80E1 // GL_BGRA_EXT: native-buffer texture is R/B swapped on Mali
    } else {
        GL_RGBA as u32
    };
    read_pixels(
        0,
        0,
        w as i32,
        h as i32,
        read_format,
        GL_UNSIGNED_BYTE,
        rgba.as_mut_ptr() as *mut c_void,
    );
    let read_done = std::time::Instant::now();

    let fd = libc::memfd_create(c"pengudroid-gleshim-shm".as_ptr(), libc::MFD_CLOEXEC);
    if fd < 0 {
        return Err(format!("memfd_create failed: {}", std::io::Error::last_os_error()));
    }
    let mut copy_done = read_done;
    let result = (|| -> Result<(), String> {
        if libc::ftruncate(fd, len as libc::off_t) != 0 {
            return Err(format!("ftruncate failed: {}", std::io::Error::last_os_error()));
        }
        let mapping = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if mapping == libc::MAP_FAILED {
            return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
        }
        let stride = w * 4;
        for y in 0..h {
            let src = (h - 1 - y) * stride;
            std::ptr::copy_nonoverlapping(
                rgba[src..src + stride].as_ptr(),
                (mapping as *mut u8).add(y * stride),
                stride,
            );
        }
        libc::munmap(mapping, len);
        copy_done = std::time::Instant::now();

        let mut st = lock_state();
        let surface_id = ensure_surface_id(&mut st, rec)?;
        let conn = st.conn.take().ok_or_else(|| "daemon is unavailable".to_string())?;
        let result = (|| -> Result<(), String> {
            let message = Message::PresentShm {
                surface_id,
                width: rec.w,
                height: rec.h,
                stride: rec.w * 4,
                format: ANDROID_RGBA_8888,
                native_window: if env_truthy("PENGUDROID_ANDROID_LAYERS") { rec.native_window as u64 } else { 0 },
                crop: BufferCrop::full(rec.w as i32, rec.h as i32),
            };
            conn.send_message(&message, &[fd]).map_err(|e| format!("PresentShm send failed: {e}"))?;
            let (reply, fds) = conn.recv_message().map_err(|e| format!("PresentShm reply failed: {e}"))?;
            close_fds(&fds);
            match reply {
                Message::Ack => Ok(()),
                Message::Error { code, message } => Err(format!("present rejected ({code}): {message}")),
                other => Err(format!("unexpected present reply: {other:?}")),
            }
        })();
        if result.is_err() {
            st.conn = None;
            st.connect_attempted = false;
        } else {
            st.conn = Some(conn);
        }
        result
    })();
    libc::close(fd);
    if profile && result.is_ok() {
        let done = std::time::Instant::now();
        profile_shm(
            read_done.duration_since(started),
            copy_done.duration_since(read_done),
            done.duration_since(copy_done),
            done.duration_since(started),
        );
    }
    result
}

#[derive(Default)]
struct ShmProfile {
    count: u32,
    since: Option<std::time::Instant>,
    read: std::time::Duration,
    copy: std::time::Duration,
    ipc: std::time::Duration,
    total: std::time::Duration,
}

fn profile_shm(
    read: std::time::Duration,
    copy: std::time::Duration,
    ipc: std::time::Duration,
    total: std::time::Duration,
) {
    static PROFILE: Mutex<ShmProfile> = Mutex::new(ShmProfile {
        count: 0, since: None,
        read: std::time::Duration::ZERO, copy: std::time::Duration::ZERO,
        ipc: std::time::Duration::ZERO, total: std::time::Duration::ZERO,
    });
    let mut p = PROFILE.lock().unwrap_or_else(|e| e.into_inner());
    p.count += 1;
    p.read += read; p.copy += copy; p.ipc += ipc; p.total += total;
    let expired = p.since.is_none_or(|since| since.elapsed() >= std::time::Duration::from_secs(1));
    if expired {
        let n = p.count.max(1) as u32;
        eprintln!(
            "[gleshim:shm] frames={} read={:.2}ms copy={:.2}ms ipc={:.2}ms total={:.2}ms",
            p.count,
            p.read.as_secs_f64() * 1000.0 / n as f64,
            p.copy.as_secs_f64() * 1000.0 / n as f64,
            p.ipc.as_secs_f64() * 1000.0 / n as f64,
            p.total.as_secs_f64() * 1000.0 / n as f64,
        );
        p.count = 0;
        p.read = std::time::Duration::ZERO; p.copy = std::time::Duration::ZERO;
        p.ipc = std::time::Duration::ZERO; p.total = std::time::Duration::ZERO;
        p.since = Some(std::time::Instant::now());
    }
}

// ---------------------------------------------------------------------------
// Exported EGL API
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn eglGetDisplay(native_display: *mut c_void) -> EGLDisplay {
    let _host_call = HostCallGuard::enter();
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return std::ptr::null_mut();
        }
    };

    let mut display = std::ptr::null_mut();
    if native_display.is_null() {
        // One surfaceless display for the whole process. The framework creates an
        // `EglManager` per window (the activity's, then a dialog's), and each one
        // calls `eglGetDisplay`, so without this cache the second window opened a
        // second display and context — two render paths over one FBO-backed
        // surface set. The host returns the same object for the surfaceless
        // platform anyway; skipping the call keeps the shim's own state single.
        let cached = lock_state().display as EGLDisplay;
        if !cached.is_null() {
            return cached;
        }
        if let Some(get_platform_display) = host.get_platform_display {
            display = get_platform_display(
                EGL_PLATFORM_SURFACELESS_MESA,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if display.is_null() {
                // Hybris supports Wayland, not Mesa's surfaceless platform.
                // Consume its probe error before the working default-display
                // fallback; otherwise HWUI sees this stale EGL_BAD_PARAMETER
                // after its first successful swap and aborts.
                let _ = (host.get_error)();
            }
        }
        if display.is_null() {
            if let Some(get_platform_display) = host.get_platform_display_ext {
                if get_platform_display as usize != eglGetPlatformDisplayEXT as usize {
                    // SAFETY: the surfaceless platform ignores both arguments.
                    display = get_platform_display(
                        EGL_PLATFORM_SURFACELESS_MESA,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                    );
                    if display.is_null() {
                        let _ = (host.get_error)();
                    }
                }
            }
        }
    }
    if display.is_null() {
        // SAFETY: `EGL_DEFAULT_DISPLAY` is the null native display.
        display = (host.get_display)(native_display);
    }

    if !display.is_null() {
        let mut st = lock_state();
        st.display = display as usize;
    }
    display
}

#[no_mangle]
pub unsafe extern "C" fn eglInitialize(
    dpy: EGLDisplay,
    major: *mut c_int,
    minor: *mut c_int,
) -> u32 {
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return EGL_FALSE;
        }
    };
    let dpy = if dpy.is_null() {
        lock_state().display as EGLDisplay
    } else {
        dpy
    };
    if dpy.is_null() {
        return EGL_FALSE;
    }
    // SAFETY: `dpy` is a display handle from the host and the out-pointers are
    // the caller's (valid or null, which EGL accepts). Hybris may query its
    // real configstore over hwbinder from inside this call.
    let _host_call = HostCallGuard::enter();
    #[cfg(target_arch = "aarch64")]
    if hybris_host() { hybris_tls::rekey_linux_binder(); }
    let ok = (host.initialize)(dpy, major, minor);
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    if hybris_host() { hybris_tls::rekey_egl(); }
    if ok == EGL_TRUE {
        lock_state().display = dpy as usize;
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!(
            "[pengudroid-gleshim] eglInitialize dpy={dpy:p} -> {ok} dma_query={} dma_export={}",
            host.export_dmabuf_query.is_some(), host.export_dmabuf_export.is_some(),
        );
        let extensions = (host.query_string)(dpy, 0x3055); // EGL_EXTENSIONS
        if !extensions.is_null() {
            let text = std::ffi::CStr::from_ptr(extensions).to_string_lossy();
            let dma: Vec<_> = text.split_whitespace()
                .filter(|name| name.contains("dma") || name.contains("image"))
                .collect();
            eprintln!("[pengudroid-gleshim] EGL image/dma extensions: {dma:?}");
        }
    }
    ok
}

#[no_mangle]
pub unsafe extern "C" fn eglTerminate(dpy: EGLDisplay) -> u32 {
    // Deliberately *not* forwarded.
    //
    // There is exactly one host display for the whole process — the surfaceless
    // platform `eglGetDisplay` hands out — and every `EglManager` in the app
    // shares it, because the framework creates one per window (the activity's,
    // then the dialog's). Forwarding `eglTerminate` let whichever teardown ran
    // first invalidate the display for everybody, and from then on every host
    // call logged
    //
    //     libEGL: validate_display:567 error 3008 (EGL_BAD_DISPLAY)
    //
    // about once per frame while the surviving renderer kept using it. The
    // display is a process-lifetime resource here, so terminating it is a no-op
    // that reports success.
    let _ = dpy;
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglGetError() -> c_int {
    match host() {
        // SAFETY: plain entry point.
        Ok(host) => (host.get_error)(),
        Err(_) => EGL_NOT_INITIALIZED,
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglQueryString(dpy: EGLDisplay, name: c_int) -> *const c_char {
    const EGL_EXTENSIONS: c_int = 0x3055;
    match host() {
        // SAFETY: forwarded verbatim.
        Ok(host) => {
            let raw = (host.query_string)(dpy, name);
            if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
                let text = if raw.is_null() { String::new() } else { std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned() };
                let has = |e: &str| text.split(' ').any(|x| x == e);
                eprintln!(
                    "[gleshim] eglQueryString dpy={dpy:p} name={name:#x} thread={} fence_sync={} image_base={} tex2d={} len={}",
                    thread_label(), has("EGL_KHR_fence_sync"), has("EGL_KHR_image_base"), has("EGL_KHR_gl_texture_2D_image"), text.len()
                );
            }
            if name != EGL_EXTENSIONS || raw.is_null() {
                return raw;
            }
            // Client extensions (EGL_NO_DISPLAY) are a different list and
            // stay untouched. Display lists are extended once per display:
            // a single process-wide cache handed Chromium its client list
            // back for the display query, hiding EGL_KHR_fence_sync and
            // friends and forcing WebView onto software bitmap draws.
            if dpy.is_null() {
                return raw;
            }
            extended_display_extensions(dpy as usize, std::ffi::CStr::from_ptr(raw))
        }
        Err(_) => std::ptr::null(),
    }
}

/// `EGL_EXTENSIONS` of `display` plus the extensions this shim implements.
/// The returned string lives for the rest of the process.
fn extended_display_extensions(display: usize, base: &std::ffi::CStr) -> *const c_char {
    static CACHE: Mutex<Vec<(usize, &'static std::ffi::CStr)>> = Mutex::new(Vec::new());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = cache.iter().find(|(d, _)| *d == display) {
        return s.as_ptr();
    }
    let s: &'static std::ffi::CStr = Box::leak(extend_extension_list(&base.to_string_lossy()).into_boxed_c_str());
    cache.push((display, s));
    s.as_ptr()
}

/// Append the extensions this shim implements on top of the host's
/// KHR_image_base to a host display extension list.
fn extend_extension_list(base: &str) -> CString {
    let mut all = base.to_string();
    // hwui hard-requires `EGL_KHR_swap_buffers_with_damage`
    // (`EglManager::initialize` aborts without it); the entry point forwards
    // to `eglSwapBuffers`, so advertising it is honest.
    for extension in ["EGL_KHR_swap_buffers_with_damage",
                      "EGL_ANDROID_get_native_client_buffer",
                      "EGL_ANDROID_image_native_buffer"] {
        if !base.split(' ').any(|e| e == extension) {
            if !all.is_empty() {
                all.push(' ');
            }
            all.push_str(extension);
        }
    }
    CString::new(all).unwrap_or_default()
}

/// `eglSwapBuffersWithDamageKHR(dpy, surface, rects, n_rects)`.
///
/// The damage rectangles are a hint for the compositor; PenguDroid presents the
/// whole surface, so this is `eglSwapBuffers` with the arguments dropped.
#[no_mangle]
pub unsafe extern "C" fn eglSwapBuffersWithDamageKHR(
    dpy: EGLDisplay,
    surface: EGLSurface,
    _rects: *const c_int,
    _n_rects: c_int,
) -> u32 {
    eglSwapBuffers(dpy, surface)
}

/// `eglSetDamageRegionKHR(dpy, surface, rects, n_rects)`.
///
/// hwui calls this before every swap when partial updates are on and aborts
/// (`LOG_ALWAYS_FATAL`) when it fails. Our surfaces are offscreen FBOs, so the
/// damage region is only a hint; accepting it is enough.
#[no_mangle]
pub unsafe extern "C" fn eglSetDamageRegionKHR(
    _dpy: EGLDisplay,
    surface: EGLSurface,
    _rects: *const c_int,
    _n_rects: c_int,
) -> u32 {
    if surface.is_null() {
        return EGL_FALSE;
    }
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglGetConfigs(
    dpy: EGLDisplay,
    configs: *mut EGLConfig,
    config_size: c_int,
    num_config: *mut c_int,
) -> u32 {
    let Ok(host) = host() else { return EGL_FALSE };
    let get_configs = host.get_configs;
    get_configs(resolve_display(dpy), configs, config_size, num_config)
}

#[no_mangle]
pub unsafe extern "C" fn eglBindTexImage(
    dpy: EGLDisplay,
    surface: EGLSurface,
    buffer: c_int,
) -> u32 {
    let Ok(host) = host() else { return EGL_FALSE };
    host.bind_tex_image.map_or(EGL_FALSE, |f| f(resolve_display(dpy), surface, buffer))
}

#[no_mangle]
pub unsafe extern "C" fn eglReleaseTexImage(
    dpy: EGLDisplay,
    surface: EGLSurface,
    buffer: c_int,
) -> u32 {
    let Ok(host) = host() else { return EGL_FALSE };
    host.release_tex_image.map_or(EGL_FALSE, |f| f(resolve_display(dpy), surface, buffer))
}

#[no_mangle]
pub unsafe extern "C" fn eglClientWaitSync(
    dpy: EGLDisplay,
    sync: EGLSync,
    flags: c_int,
    timeout: u64,
) -> c_int {
    let Ok(host) = host() else { return 0 };
    host.client_wait_sync.map_or(0, |f| f(resolve_display(dpy), sync, flags, timeout))
}

#[no_mangle]
pub unsafe extern "C" fn eglCreatePbufferFromClientBuffer(
    dpy: EGLDisplay,
    buffer_type: c_int,
    buffer: EGLClientBuffer,
    config: EGLConfig,
    attribs: *const c_int,
) -> EGLSurface {
    let Ok(host) = host() else { return std::ptr::null_mut() };
    host.create_pbuffer_from_client_buffer.map_or(std::ptr::null_mut(), |f| {
        f(resolve_display(dpy), buffer_type, buffer, config, attribs)
    })
}

#[no_mangle]
pub unsafe extern "C" fn eglCreatePlatformWindowSurface(
    dpy: EGLDisplay,
    config: EGLConfig,
    native_window: *mut c_void,
    _attribs: *const isize,
) -> EGLSurface {
    // This is still an Android native window, so it needs our FBO-backed
    // implementation rather than a host platform surface.
    eglCreateWindowSurface(dpy, config, native_window, std::ptr::null())
}

#[no_mangle]
pub unsafe extern "C" fn eglCreateSync(
    dpy: EGLDisplay,
    sync_type: c_int,
    attribs: *const isize,
) -> EGLSync {
    let Ok(host) = host() else { return std::ptr::null_mut() };
    host.create_sync.map_or(std::ptr::null_mut(), |f| f(resolve_display(dpy), sync_type, attribs))
}

#[no_mangle]
pub unsafe extern "C" fn eglDestroySync(dpy: EGLDisplay, sync: EGLSync) -> u32 {
    let Ok(host) = host() else { return EGL_FALSE };
    host.destroy_sync.map_or(EGL_FALSE, |f| f(resolve_display(dpy), sync))
}

#[no_mangle]
pub unsafe extern "C" fn eglGetSyncAttrib(
    dpy: EGLDisplay,
    sync: EGLSync,
    attribute: c_int,
    value: *mut isize,
) -> u32 {
    let Ok(host) = host() else { return EGL_FALSE };
    host.get_sync_attrib.map_or(EGL_FALSE, |f| f(resolve_display(dpy), sync, attribute, value))
}

#[no_mangle]
pub unsafe extern "C" fn eglQueryContext(
    dpy: EGLDisplay,
    context: EGLContext,
    attribute: c_int,
    value: *mut c_int,
) -> u32 {
    let Ok(host) = host() else { return EGL_FALSE };
    host.query_context.map_or(EGL_FALSE, |f| f(resolve_display(dpy), context, attribute, value))
}

#[no_mangle]
pub unsafe extern "C" fn eglWaitClient() -> u32 {
    host().ok().and_then(|host| host.wait_client).map_or(EGL_FALSE, |f| f())
}

#[no_mangle]
pub unsafe extern "C" fn eglWaitGL() -> u32 {
    host().ok().and_then(|host| host.wait_gl).map_or(EGL_FALSE, |f| f())
}

#[no_mangle]
pub unsafe extern "C" fn eglWaitNative(engine: c_int) -> u32 {
    host().ok().and_then(|host| host.wait_native).map_or(EGL_FALSE, |f| f(engine))
}

#[no_mangle]
pub unsafe extern "C" fn eglWaitSync(
    dpy: EGLDisplay,
    sync: EGLSync,
    flags: c_int,
) -> u32 {
    let Ok(host) = host() else { return EGL_FALSE };
    host.wait_sync.map_or(EGL_FALSE, |f| f(resolve_display(dpy), sync, flags))
}

#[no_mangle]
pub unsafe extern "C" fn eglChooseConfig(
    dpy: EGLDisplay,
    attribs: *const c_int,
    configs: *mut EGLConfig,
    config_size: c_int,
    num_config: *mut c_int,
) -> u32 {
    // A Bionic app asks for `EGL_WINDOW_BIT` because it expects a real window.
    // PenguDroid's display is surfaceless (frames are FBOs exported as
    // dma-bufs), so the request is rewritten to `EGL_PBUFFER_BIT`; otherwise the
    // host reports "no matching config" and the app cannot create a context.
    const EGL_SURFACE_TYPE: c_int = 0x3033;
    const EGL_PBUFFER_BIT: c_int = 0x0001;
    const EGL_RENDERABLE_TYPE: c_int = 0x3040;
    const EGL_OPENGL_ES2_BIT: c_int = 0x0004;

    let trace = std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some();
    let Some(original) = read_attribs(attribs) else {
        return match host() {
            // SAFETY: forwarded verbatim; the caller owns the out-pointers.
            Ok(host) => (host.choose_config)(dpy, attribs, configs, config_size, num_config),
            Err(_) => EGL_FALSE,
        };
    };

    let trace_list = |label: &str, list: &[c_int]| {
        if trace {
            let mut pairs = Vec::new();
            for pair in list.chunks(2) {
                if pair.len() == 2 {
                    pairs.push(format!("({:#x},{:#x})", pair[0], pair[1]));
                }
            }
            eprintln!("[gleshim] eglChooseConfig {label}=[{}]", pairs.join(" "));
        }
    };

    // The requested sizes are advisory on PenguDroid: the shim renders into its
    // own FBO and exports that, so a 32-bit depth buffer the host cannot offer
    // must not fail window creation. Real games ask for `EGL_DEPTH_SIZE=32`,
    // which llvmpipe (24-bit) answers with zero configs.
    const RELAXABLE: [c_int; 10] = [
        0x3020, // EGL_BUFFER_SIZE
        0x3024, // EGL_RED_SIZE
        0x3023, // EGL_GREEN_SIZE
        0x3022, // EGL_BLUE_SIZE
        0x3021, // EGL_ALPHA_SIZE
        0x3025, // EGL_DEPTH_SIZE
        0x3026, // EGL_STENCIL_SIZE
        0x3032, // EGL_SAMPLES
        0x3031, // EGL_SAMPLE_BUFFERS
        0x3029, // EGL_LEVEL
    ];

    let mut exact = original.clone();
    for pair in exact.chunks_mut(2) {
        if pair.len() == 2 && pair[0] == EGL_SURFACE_TYPE {
            pair[1] = EGL_PBUFFER_BIT;
        }
    }
    // Mesa Intel/AMD surfaceless displays include RGB565 configs that games like
    // SDL3 pick by mistake when attrs omit alpha/bits or ask for >= 5 bits.
    // Try an explicit RGBA8888 preference first, falling back to `exact`/`relaxed`.
    let mut rgba8 = Vec::with_capacity(exact.len() + 10);
    let (mut has_r, mut has_g, mut has_b, mut has_a) = (false, false, false, false);
    for pair in exact.chunks(2) {
        if pair.len() < 2 {
            break;
        }
        let k = pair[0];
        let mut v = pair[1];
        match k {
            0x3020 if v > 0 && v < 32 => v = 32,
            0x3024 => {
                has_r = true;
                if v < 8 { v = 8; }
            }
            0x3023 => {
                has_g = true;
                if v < 8 { v = 8; }
            }
            0x3022 => {
                has_b = true;
                if v < 8 { v = 8; }
            }
            0x3021 => {
                has_a = true;
                if v < 8 { v = 8; }
            }
            _ => {}
        }
        rgba8.extend([k, v]);
    }
    if !has_r { rgba8.extend([0x3024, 8]); }
    if !has_g { rgba8.extend([0x3023, 8]); }
    if !has_b { rgba8.extend([0x3022, 8]); }
    if !has_a { rgba8.extend([0x3021, 8]); }
    rgba8.push(EGL_NONE);

    let mut relaxed = exact.clone();
    for pair in relaxed.chunks_mut(2) {
        if pair.len() == 2 && RELAXABLE.contains(&pair[0]) {
            pair[1] = 0;
        }
    }
    let mut minimal = vec![
        EGL_SURFACE_TYPE,
        EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE,
        EGL_OPENGL_ES2_BIT,
        EGL_NONE,
    ];
    // Keep the app's renderable type: an ES3 app must not be handed an ES2-only
    // config, or `eglCreateContext` fails one step later.
    if let Some(index) = exact.chunks(2).position(|pair| pair.len() == 2 && pair[0] == EGL_RENDERABLE_TYPE) {
        minimal[3] = exact[index * 2 + 1];
    }

    let attempts: [(&str, &Vec<c_int>); 4] = [
        ("rgba8", &rgba8),
        ("req", &exact),
        ("relaxed", &relaxed),
        ("minimal", &minimal),
    ];

    let mut last = EGL_FALSE;
    for (label, list) in attempts {
        trace_list(label, list);
        let Ok(host) = host() else {
            return EGL_FALSE;
        };
        // SAFETY: every list is `EGL_NONE`-terminated by construction and the
        // out-pointers belong to the caller.
        let result = (host.choose_config)(dpy, list.as_ptr(), configs, config_size, num_config);
        let count = if num_config.is_null() { 0 } else { *num_config };
        if trace {
            eprintln!("[gleshim] eglChooseConfig {label} -> {result} count={count}");
        }
        last = result;
        if result != EGL_FALSE && count > 0 {
            return result;
        }
    }
    last
}

#[no_mangle]
pub unsafe extern "C" fn eglGetConfigAttrib(
    dpy: EGLDisplay,
    config: EGLConfig,
    attribute: c_int,
    value: *mut c_int,
) -> u32 {
    const EGL_NATIVE_VISUAL_ID: c_int = 0x302E;
    const WINDOW_FORMAT_RGBA_8888: c_int = 1;
    if attribute == EGL_NATIVE_VISUAL_ID && !value.is_null() {
        *value = WINDOW_FORMAT_RGBA_8888;
        return EGL_TRUE;
    }
    match host() {
        // SAFETY: forwarded verbatim.
        Ok(host) => (host.get_config_attrib)(dpy, config, attribute, value),
        Err(_) => EGL_FALSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglCreateContext(
    dpy: EGLDisplay,
    config: EGLConfig,
    share_context: EGLContext,
    attribs: *const c_int,
) -> EGLContext {
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() && !attribs.is_null() {
        let mut pairs = Vec::new();
        for index in 0..64 {
            let key = *attribs.add(index * 2);
            if key == 0x3038 { break; }
            pairs.push((format!("{key:#x}"), format!("{:#x}", *attribs.add(index * 2 + 1))));
        }
        eprintln!("[gleshim] eglCreateContext attrs={pairs:?}");
    }
    // Android 17 HWUI sends EGL_TELEMETRY_HINT_ANDROID even when the host
    // display does not advertise it. It controls Android GPU-stats collection,
    // which this host renderer does not perform. Consume this one hint here;
    // retain every rendering/safety attribute for the host's own validation.
    let mut translated = Vec::new();
    if let Some(original) = read_attribs(attribs) {
        let mut index = 0;
        while index + 1 < original.len() {
            let key = original[index];
            let value = original[index + 1];
            if key != 0x3570 { translated.extend([key, value]); }
            index += 2;
        }
        translated.push(EGL_NONE);
    }
    let attribs = if translated.is_empty() { attribs } else { translated.as_ptr() };
    match host() {
        // SAFETY: forwarded verbatim.
        Ok(host) => {
            let context = (host.create_context)(dpy, config, share_context, attribs);
            #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
            if hybris_host() { hybris_tls::rekey_egl(); }
            register_context(context as usize, share_context as usize);
            if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
                eprintln!("[gleshim] eglCreateContext share={share_context:p} -> {context:p} thread={}", thread_label());
            }
            context
        }
        Err(e) => {
            log_once(format_args!("{e}"));
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> u32 {
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglDestroyContext ctx={ctx:p} thread={}", thread_label());
    }
    let ok = match host() {
        // SAFETY: forwarded verbatim.
        Ok(host) => (host.destroy_context)(dpy, ctx),
        Err(_) => EGL_FALSE,
    };
    if ok == EGL_TRUE {
        forget_context(ctx as usize);
        let mut st = lock_state();
        if st.context == ctx as usize {
            st.context = 0;
        }
        CURRENT_CONTEXT.with(|c| if c.get() == ctx as usize { c.set(0) });
    }
    ok
}

#[no_mangle]
pub unsafe extern "C" fn eglMakeCurrent(
    dpy: EGLDisplay,
    draw: EGLSurface,
    read: EGLSurface,
    ctx: EGLContext,
) -> u32 {
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return EGL_FALSE;
        }
    };

    // A surfaceless host display has no window surfaces; the app renders into
    // our FBO instead, so only the context is made current.
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglMakeCurrent dpy={dpy:p} draw={draw:p} ctx={ctx:p} thread={}", thread_label());
    }
    let dpy = resolve_display(dpy);
    let ok = (host.make_current)(
        dpy,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        ctx,
    );
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    if hybris_host() { hybris_tls::rekey_egl(); }
    if ok != EGL_TRUE {
        // A failed eglMakeCurrent leaves the previous binding in place.
        return EGL_FALSE;
    }

    {
        let mut st = lock_state();
        if !dpy.is_null() {
            st.display = dpy as usize;
        }
        if !ctx.is_null() {
            st.context = ctx as usize;
        }
    }
    CURRENT_CONTEXT.with(|c| c.set(ctx as usize));
    CURRENT_SURFACE.with(|c| c.set(draw as usize));

    // `read` is ignored: the FBO is both the draw and read target.
    let _ = read;

    if !draw.is_null() {
        // Binding has to happen after the context is current. `draw` is one of
        // our `SurfaceRec`s; `ensure_fbo`/`bind_surface_fbo` need no lock.
        if let Err(e) = make_current_surface_fbo(draw, host) {
            log_once(format_args!("eglMakeCurrent: {e}"));
            return EGL_FALSE;
        }
    }
    EGL_TRUE
}

/// `tid/comm` of the calling thread, for traces.
fn thread_label() -> String {
    // SAFETY: gettid has no preconditions.
    let tid = unsafe { libc::gettid() };
    let comm = std::fs::read_to_string(format!("/proc/self/task/{tid}/comm")).unwrap_or_default();
    format!("{tid}/{}", comm.trim())
}

/// Under `PENGUDROID_TRACE_GLESHIM`, report the first `what` call per thread.
fn trace_thread_once(what: &'static str) {
    thread_local!(static SEEN: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) });
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_none() {
        return;
    }
    if SEEN.with(|s| {
        let mut s = s.borrow_mut();
        let new = !s.contains(&what);
        if new {
            s.push(what);
        }
        new
    }) {
        eprintln!("[gleshim] first {what} on thread {}", thread_label());
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglGetCurrentContext() -> EGLContext {
    trace_thread_once("eglGetCurrentContext");
    if let Some(get_current_context) = host().ok().and_then(|h| h.get_current_context) {
        // SAFETY: plain entry point.
        return get_current_context();
    }
    CURRENT_CONTEXT.with(|c| c.get()) as EGLContext
}

#[no_mangle]
pub unsafe extern "C" fn eglGetCurrentDisplay() -> EGLDisplay {
    if let Some(get_current_display) = host().ok().and_then(|h| h.get_current_display) {
        // SAFETY: plain entry point.
        let display = get_current_display();
        if !display.is_null() {
            return display;
        }
    }
    lock_state().display as EGLDisplay
}

#[no_mangle]
pub unsafe extern "C" fn eglGetCurrentSurface(_readdraw: i32) -> EGLSurface {
    current_surface() as EGLSurface
}

#[no_mangle]
pub unsafe extern "C" fn eglBindAPI(api: u32) -> u32 {
    match host().ok().and_then(|h| h.bind_api) {
        // SAFETY: forwarded verbatim.
        Some(bind_api) => bind_api(api),
        None => EGL_FALSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglSwapInterval(dpy: EGLDisplay, interval: c_int) -> u32 {
    match host().ok().and_then(|h| h.swap_interval) {
        // SAFETY: forwarded verbatim.
        Some(swap_interval) => swap_interval(resolve_display(dpy), interval),
        None => EGL_FALSE,
    }
}

// ---------------------------------------------------------------------------
// EGL_ANDROID_image_native_buffer on a host EGL that has never heard of it
// ---------------------------------------------------------------------------

const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_GL_TEXTURE_2D_KHR_TARGET: u32 = 0x30B1;

/// `AHardwareBuffer_Desc`'s leading fields (android/hardware_buffer.h).
#[repr(C)]
#[derive(Default)]
struct AhbDesc { width: u32, height: u32, layers: u32, format: u32, usage: u64, stride: u32, rfu0: u32, rfu1: u64 }

/// One GPU image per AHardwareBuffer, shared by every `eglCreateImage` on it,
/// so that all contexts (a WebGL producer and the compositor that samples it)
/// see the same storage. Each create returns the same EGLImage and takes a
/// reference; the last destroy releases it.
struct NativeImage { image: usize, references: usize }
fn native_images() -> &'static Mutex<std::collections::HashMap<usize, NativeImage>> {
    static IMAGES: OnceLock<Mutex<std::collections::HashMap<usize, NativeImage>>> = OnceLock::new();
    IMAGES.get_or_init(Default::default)
}

/// GL internal format, format and type for an AHardwareBuffer format.
fn gl_format_for_ahb(format: u32) -> Option<(c_int, u32, u32)> {
    const GL_RGB: u32 = 0x1907;
    const GL_RED: u32 = 0x1903;
    match format {
        1 | 2 => Some((GL_RGBA8, GL_RGBA as u32, GL_UNSIGNED_BYTE)),     // R8G8B8A8 / R8G8B8X8
        3 => Some((0x8051, GL_RGB, GL_UNSIGNED_BYTE)),                   // R8G8B8 -> GL_RGB8
        4 => Some((0x8D62, GL_RGB, 0x8363)),                             // R5G6B5 -> GL_RGB565
        0x16 => Some((0x881A, GL_RGBA as u32, 0x140B)),                  // R16G16B16A16_FLOAT
        0x2b => Some((0x8059, GL_RGBA as u32, 0x8368)),                  // R10G10B10A2 (2_10_10_10_REV)
        0x38 => Some((0x8229, GL_RED, GL_UNSIGNED_BYTE)),                // R8_UNORM -> GL_R8
        _ => None,
    }
}

/// `eglGetNativeClientBufferANDROID(buffer)`: the AHardwareBuffer itself is the
/// client buffer; `eglCreateImageKHR` below knows how to read it.
#[no_mangle]
pub extern "C" fn eglGetNativeClientBufferANDROID(buffer: *const c_void) -> EGLClientBuffer {
    buffer as EGLClientBuffer
}

unsafe fn create_native_buffer_image(host: &Host, dpy: EGLDisplay, buffer: EGLClientBuffer) -> EGLImageKHR {
    let key = buffer as usize;
    if let Some(entry) = native_images().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&key) {
        entry.references += 1;
        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            eprintln!("[gleshim] AHardwareBuffer {buffer:p} reused EGLImage {:#x} (refs {}, tid {})",
                entry.image, entry.references, libc::gettid());
        }
        return entry.image as EGLImageKHR;
    }
    type Describe = unsafe extern "C" fn(*const c_void, *mut AhbDesc);
    let describe = libc::dlsym(libc::RTLD_DEFAULT, c"AHardwareBuffer_describe".as_ptr());
    let context = host.get_current_context.map(|f| f()).unwrap_or(std::ptr::null_mut());
    let (Some(create), Some(gen), Some(bind), Some(upload), Some(parameter), false, false) = (
        host.create_image_khr, host.gl_gen_textures, host.gl_bind_texture, host.gl_tex_image_2d,
        host.gl_tex_parameter_i, describe.is_null(), context.is_null(),
    ) else {
        log_once(format_args!("EGL_NATIVE_BUFFER_ANDROID: no AHardwareBuffer_describe or no current context"));
        return std::ptr::null_mut();
    };
    let mut desc = AhbDesc::default();
    (std::mem::transmute::<*mut c_void, Describe>(describe))(buffer, &mut desc);
    let Some((internal, format, kind)) = gl_format_for_ahb(desc.format) else {
        log_once(format_args!("EGL_NATIVE_BUFFER_ANDROID: unsupported AHardwareBuffer format {:#x}", desc.format));
        return std::ptr::null_mut();
    };
    if desc.width == 0 || desc.height == 0 || desc.width > 16384 || desc.height > 16384 {
        return std::ptr::null_mut();
    }
    // The texture only seeds the image: the image keeps the storage alive,
    // so the name can go at once and nothing is tied to this context.
    let mut texture = 0u32;
    gen(1, &mut texture);
    bind(GL_TEXTURE_2D, texture);
    upload(GL_TEXTURE_2D, 0, internal, desc.width as c_int, desc.height as c_int, 0, format, kind, std::ptr::null());
    parameter(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    parameter(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    let image = create(dpy, context, EGL_GL_TEXTURE_2D_KHR_TARGET, texture as usize as EGLClientBuffer, std::ptr::null());
    type Delete = unsafe extern "C" fn(c_int, *const u32);
    let delete = eglGetProcAddress(c"glDeleteTextures".as_ptr());
    if !delete.is_null() { (std::mem::transmute::<*mut c_void, Delete>(delete))(1, &texture); }
    if image.is_null() {
        log_once(format_args!("EGL_NATIVE_BUFFER_ANDROID: host eglCreateImageKHR(texture) failed"));
        return image;
    }
    native_images().lock().unwrap_or_else(|e| e.into_inner())
        .insert(key, NativeImage { image: image as usize, references: 1 });
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] AHardwareBuffer {buffer:p} {}x{} format {:#x} usage {:#x} -> EGLImage {image:p} (ctx {context:p}, tid {})",
            desc.width, desc.height, desc.format, desc.usage, libc::gettid());
    }
    image
}

/// `eglCreateImageKHR`, with `EGL_NATIVE_BUFFER_ANDROID` served here.
#[no_mangle]
pub unsafe extern "C" fn eglCreateImageKHR(dpy: EGLDisplay, ctx: EGLContext, target: u32,
    buffer: EGLClientBuffer, attribs: *const c_int) -> EGLImageKHR {
    let Ok(host) = host() else { return std::ptr::null_mut() };
    let dpy = resolve_display(dpy);
    if target == EGL_NATIVE_BUFFER_ANDROID {
        return create_native_buffer_image(host, dpy, buffer);
    }
    let image = match host.create_image_khr {
        Some(create) => create(dpy, ctx, target, buffer, attribs),
        None => std::ptr::null_mut(),
    };
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglCreateImageKHR target={target:#x} ctx={ctx:p} buffer={buffer:p} -> {image:p} thread={}", thread_label());
    }
    image
}

/// `eglDestroyImageKHR`: images made from an AHardwareBuffer are reference counted.
#[no_mangle]
pub unsafe extern "C" fn eglDestroyImageKHR(dpy: EGLDisplay, image: EGLImageKHR) -> u32 {
    let Ok(host) = host() else { return EGL_FALSE };
    let dpy = resolve_display(dpy);
    {
        let mut images = native_images().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((&key, entry)) = images.iter_mut().find(|(_, e)| e.image == image as usize) {
            entry.references -= 1;
            if entry.references > 0 { return EGL_TRUE; }
            images.remove(&key);
        }
    }
    match host.destroy_image_khr {
        Some(destroy) => destroy(dpy, image),
        None => EGL_FALSE,
    }
}

/// EGL 1.5 `eglCreateImage`: same as the KHR entry point, with `EGLAttrib`s.
#[no_mangle]
pub unsafe extern "C" fn eglCreateImage(dpy: EGLDisplay, ctx: EGLContext, target: u32,
    buffer: EGLClientBuffer, attribs: *const isize) -> EGLImageKHR {
    if target == EGL_NATIVE_BUFFER_ANDROID {
        let Ok(host) = host() else { return std::ptr::null_mut() };
        return create_native_buffer_image(host, resolve_display(dpy), buffer);
    }
    // Narrow the EGLAttrib list to the KHR entry point's EGLint list.
    let mut narrowed = Vec::new();
    if !attribs.is_null() {
        for index in 0..64 {
            let key = *attribs.add(index * 2);
            narrowed.push(key as c_int);
            if key as c_int == EGL_NONE { break; }
            narrowed.push(*attribs.add(index * 2 + 1) as c_int);
        }
    }
    eglCreateImageKHR(dpy, ctx, target, buffer, if attribs.is_null() { std::ptr::null() } else { narrowed.as_ptr() })
}

#[no_mangle]
pub unsafe extern "C" fn eglDestroyImage(dpy: EGLDisplay, image: EGLImageKHR) -> u32 {
    eglDestroyImageKHR(dpy, image)
}

#[no_mangle]
pub unsafe extern "C" fn eglGetProcAddress(name: *const c_char) -> *mut c_void {
    thread_local! {
        static IN_HOST_QUERY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    struct QueryGuard;
    impl Drop for QueryGuard {
        fn drop(&mut self) {
            IN_HOST_QUERY.with(|active| active.set(false));
        }
    }
    // Aurora's host EGL can resolve an extension by dlopen'ing libEGL.so and
    // calling the just-resolved public eglGetProcAddress again. Since libEGL.so
    // is this shim, forwarding that nested query back to the host recurses
    // forever. Answer our own exports above; a nested unknown query is absent,
    // not a reason to re-enter the host loader.
    if IN_HOST_QUERY.with(|active| active.replace(true)) {
        return std::ptr::null_mut();
    }
    let _query_guard = QueryGuard;
    if name.is_null() {
        return std::ptr::null_mut();
    }
    // Answer for the entry points this shim implements itself. hwui resolves
    // `eglSetDamageRegionKHR`/`eglSwapBuffersWithDamageKHR` through here, and
    // the host's copies reject our offscreen surfaces with `EGL_BAD_DISPLAY`.
    let requested = std::ffi::CStr::from_ptr(name).to_bytes();
    let ours: &[(&[u8], *mut c_void)] = &[
        (b"eglGetDisplay", eglGetDisplay as *mut c_void),
        (b"eglGetPlatformDisplay", eglGetPlatformDisplay as *mut c_void),
        (b"eglGetPlatformDisplayEXT", eglGetPlatformDisplayEXT as *mut c_void),
        (b"eglInitialize", eglInitialize as *mut c_void),
        (b"eglTerminate", eglTerminate as *mut c_void),
        (b"eglGetError", eglGetError as *mut c_void),
        (b"eglQueryString", eglQueryString as *mut c_void),
        (b"eglChooseConfig", eglChooseConfig as *mut c_void),
        (b"eglGetConfigAttrib", eglGetConfigAttrib as *mut c_void),
        (b"eglCreateContext", eglCreateContext as *mut c_void),
        (b"eglDestroyContext", eglDestroyContext as *mut c_void),
        (b"eglMakeCurrent", eglMakeCurrent as *mut c_void),
        (b"eglGetCurrentContext", eglGetCurrentContext as *mut c_void),
        (b"eglGetCurrentDisplay", eglGetCurrentDisplay as *mut c_void),
        (b"eglGetCurrentSurface", eglGetCurrentSurface as *mut c_void),
        (b"eglBindAPI", eglBindAPI as *mut c_void),
        (b"eglQueryAPI", eglQueryAPI as *mut c_void),
        (b"eglSwapInterval", eglSwapInterval as *mut c_void),
        (b"eglCreatePbufferSurface", eglCreatePbufferSurface as *mut c_void),
        (b"eglSetDamageRegionKHR", eglSetDamageRegionKHR as *mut c_void),
        (b"eglSwapBuffersWithDamageKHR", eglSwapBuffersWithDamageKHR as *mut c_void),
        (b"eglSurfaceAttrib", eglSurfaceAttrib as *mut c_void),
        (b"eglQuerySurface", eglQuerySurface as *mut c_void),
        (b"eglCreateWindowSurface", eglCreateWindowSurface as *mut c_void),
        (b"eglDestroySurface", eglDestroySurface as *mut c_void),
        (b"eglSwapBuffers", eglSwapBuffers as *mut c_void),
        (b"glBindFramebuffer", glBindFramebuffer as *mut c_void),
        (b"eglGetNativeClientBufferANDROID", eglGetNativeClientBufferANDROID as *mut c_void),
        (b"eglCreateImageKHR", eglCreateImageKHR as *mut c_void),
        (b"eglDestroyImageKHR", eglDestroyImageKHR as *mut c_void),
        (b"eglCreateImage", eglCreateImage as *mut c_void),
        (b"eglDestroyImage", eglDestroyImage as *mut c_void),
        (b"eglCreateSyncKHR", eglCreateSyncKHR as *mut c_void),
        (b"eglDestroySyncKHR", eglDestroySyncKHR as *mut c_void),
        (b"eglClientWaitSyncKHR", eglClientWaitSyncKHR as *mut c_void),
        (b"eglWaitSyncKHR", eglWaitSyncKHR as *mut c_void),
        (b"eglGetSyncAttribKHR", eglGetSyncAttribKHR as *mut c_void),
        (b"eglDupNativeFenceFDANDROID", eglDupNativeFenceFDANDROID as *mut c_void),
        (b"eglPresentationTimeANDROID", eglPresentationTimeANDROID as *mut c_void),
    ];
    for (symbol, ptr) in ours {
        if requested == *symbol {
            return *ptr;
        }
    }
    if HOST_LOADING.with(|loading| loading.get()) {
        return std::ptr::null_mut();
    }
    let host = match host() {
        Ok(host) => host,
        Err(_) => return std::ptr::null_mut(),
    };
    if let Some(get_proc_address) = host.get_proc_address {
        // SAFETY: `name` is the caller's NUL-terminated symbol name.
        let ptr = get_proc_address(name);
        if !ptr.is_null() {
            return ptr;
        }
    }
    if host.gles_handle != 0 {
        // SAFETY: the handle came from dlopen and `name` is a valid C string.
        let ptr = libc::dlsym(host.gles_handle as *mut c_void, name);
        if !ptr.is_null() {
            return ptr;
        }
    }
    std::ptr::null_mut()
}

/// Ask an `ANativeWindow` for its buffer dimensions.
///
/// `ANativeWindow_getWidth`/`getHeight` live in Bionic's `libandroid.so`, which
/// the app has already loaded by the time hwui creates a surface. They are
/// resolved lazily so a process without `libandroid.so` still works.
unsafe fn window_size(window: *mut c_void) -> (u32, u32) {
    if window.is_null() {
        return (0, 0);
    }
    type GetDim = unsafe extern "C" fn(*mut c_void) -> i32;
    let mut resolve = |name: &[u8]| -> Option<GetDim> {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char);
        if sym.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, GetDim>(sym))
        }
    };
    let w = resolve(b"ANativeWindow_getWidth\0").map(|f| f(window)).unwrap_or(0);
    let h = resolve(b"ANativeWindow_getHeight\0").map(|f| f(window)).unwrap_or(0);
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] ANativeWindow reports {w}x{h}");
    }
    if w > 0 && h > 0 {
        (w as u32, h as u32)
    } else {
        (0, 0)
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglCreateWindowSurface(
    _dpy: EGLDisplay,
    _config: EGLConfig,
    native_window: *mut c_void,
    _attribs: *const c_int,
) -> EGLSurface {
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglCreateWindowSurface window={native_window:p}");
    }
    // Bionic hands us an `ANativeWindow*`. There is no host window, only the
    // FBO that `eglSwapBuffers` exports, but the window still knows the size the
    // framework laid the surface out at — and hwui renders at that size. Using
    // the window's dimensions keeps the FBO, the viewport and the presented
    // buffer in agreement; the env vars are only a fallback.
    let (mut w, mut h) = window_size(native_window);
    if w == 0 || h == 0 {
        w = env_dim("PENGUDROID_SURFACE_W", 720);
        h = env_dim("PENGUDROID_SURFACE_H", 1280);
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglCreateWindowSurface -> {w}x{h}");
    }
    let early_surface = std::env::var("PENGUDROID_SURFACE_ID")
        .ok().and_then(|value| value.trim().parse::<u32>().ok());
    if let Some(id) = early_surface {
        eprintln!("[pengudroid-gleshim] window attaches to early surface {id}");
        subscribe_input(id);
    }
    // A login/navigation tap can replace the Android window after its input
    // event has already been dispatched. With present-driven vsync there may
    // be no outstanding frame left to wake the new window's first traversal.
    // Issue the same bounded wakeup used for input; it is rate-limited in the
    // shim and never becomes a free-running producer.
    notify_input_wakeup_vsync();
    let rec = Box::new(SurfaceRec {
        w,
        h,
        targets: Vec::new(),
        surface_id: early_surface.unwrap_or(0),
        owns_surface: false,
        native_window: native_window as usize,
        layer_resolved: false,
        child_of_early: false,
    });
    let surface = Box::into_raw(rec) as EGLSurface;
    live_surfaces().lock().unwrap_or_else(|e| e.into_inner()).insert(surface as usize);
    surface
}

/// Complete the request/reply exchange before another swap uses this socket.
/// On transport failure discard it: a late Ack must never answer a later present.
fn destroy_daemon_surface(st: &mut State, rec: &mut SurfaceRec) -> Result<(), String> {
    if !rec.owns_surface || rec.surface_id == 0 {
        return Ok(());
    }
    let id = std::mem::replace(&mut rec.surface_id, 0);
    rec.owns_surface = false;
    let Some(conn) = st.conn.take() else { return Ok(()) };
    if let Err(e) = conn.send_message(&Message::DestroySurface { surface_id: id }, &[]) {
        st.connect_attempted = false;
        return Err(format!("DestroySurface send failed: {e}"));
    }
    let (reply, fds) = match conn.recv_message() {
        Ok(reply) => reply,
        Err(e) => {
            st.connect_attempted = false;
            return Err(format!("DestroySurface reply failed: {e}"));
        }
    };
    close_fds(&fds);
    st.conn = Some(conn);
    match reply {
        Message::Ack => Ok(()),
        other => Err(format!("unexpected DestroySurface reply: {other:?}")),
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglDestroySurface(_dpy: EGLDisplay, surface: EGLSurface) -> u32 {
    if surface.is_null() {
        return EGL_FALSE;
    }
    {
        live_surfaces().lock().unwrap_or_else(|e| e.into_inner()).remove(&(surface as usize));
        CURRENT_SURFACE.with(|c| if c.get() == surface as usize { c.set(0) });
        let mut st = lock_state();
        let rec = &mut *(surface as *mut SurfaceRec);
        if let Ok(h) = host() {
            if h.get_current_context.is_some_and(|cur| !cur().is_null()) {
                if let Some(del_rb) = h.gl_delete_renderbuffers {
                    for target in &rec.targets {
                        if target.depth_rbo != 0 {
                            del_rb(1, &target.depth_rbo);
                        }
                    }
                }
            }
        }
        if let Err(e) = destroy_daemon_surface(&mut st, rec) {
            eprintln!("[pengudroid-gleshim] {e}");
        }
    }
    // SAFETY: `surface` was produced by `Box::into_raw` in
    // `eglCreateWindowSurface` and is dropped exactly once here.
    drop(Box::from_raw(surface as *mut SurfaceRec));
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglCreatePbufferSurface(
    _dpy: EGLDisplay,
    _config: EGLConfig,
    attribs: *const c_int,
) -> EGLSurface {
    // A pbuffer is just another offscreen target for us: the same FBO-backed
    // surface, with geometry taken from EGL_WIDTH/EGL_HEIGHT if supplied.
    const EGL_WIDTH: c_int = 0x3057;
    const EGL_HEIGHT: c_int = 0x3056;
    let mut w = env_dim("PENGUDROID_SURFACE_W", 720);
    let mut h = env_dim("PENGUDROID_SURFACE_H", 1280);
    if !attribs.is_null() {
        if let Some(list) = read_attribs(attribs) {
            let mut i = 0;
            while i + 1 < list.len() {
                let key = list[i];
                let val = list[i + 1];
                if key == EGL_WIDTH && val > 0 {
                    w = val as u32;
                } else if key == EGL_HEIGHT && val > 0 {
                    h = val as u32;
                }
                i += 2;
            }
        }
    }
    let rec = Box::new(SurfaceRec {
        w, h, targets: Vec::new(), surface_id: 0, owns_surface: false,
        native_window: 0, layer_resolved: true, child_of_early: false,
    });
    let surface = Box::into_raw(rec) as EGLSurface;
    live_surfaces().lock().unwrap_or_else(|e| e.into_inner()).insert(surface as usize);
    surface
}

#[no_mangle]
pub unsafe extern "C" fn eglSurfaceAttrib(
    _dpy: EGLDisplay,
    surface: EGLSurface,
    attribute: c_int,
    value: c_int,
) -> u32 {
    // hwui sets `EGL_SWAP_BEHAVIOR` to `EGL_BUFFER_DESTROYED` on every surface it
    // creates and aborts when that fails. Our surfaces are offscreen FBOs, so the
    // attribute has no meaning; accepting it (and answering the query) is what
    // keeps the render thread alive.
    if surface.is_null() {
        return EGL_FALSE;
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglSurfaceAttrib surface={surface:p} attr=0x{attribute:x} value=0x{value:x}");
    }
    let _ = (attribute, value);
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglQuerySurface(
    dpy: EGLDisplay,
    surface: EGLSurface,
    attribute: c_int,
    value: *mut c_int,
) -> u32 {
    let _ = resolve_display(dpy);
    if surface.is_null() || value.is_null() {
        return EGL_FALSE;
    }
    // SAFETY: non-null surfaces are `SurfaceRec` pointers from the create calls.
    let rec = &*(surface as *const SurfaceRec);
    match attribute {
        0x3057 => {
            *value = rec.w as c_int; // EGL_WIDTH
            EGL_TRUE
        }
        0x3056 => {
            *value = rec.h as c_int; // EGL_HEIGHT
            EGL_TRUE
        }
        // EGL_SWAP_BEHAVIOR -> EGL_BUFFER_DESTROYED. hwui queries this right
        // after setting it; answering EGL_FALSE makes it log "Unable to match
        // the desired swap behavior" and fall back to allocating its own
        // GraphicBuffers, which needs a gralloc mapper we do not have.
        0x3093 => {
            *value = 0x3095; // EGL_BUFFER_DESTROYED
            EGL_TRUE
        }
        // EGL_BUFFER_AGE_EXT: a single-buffered offscreen FBO is always fresh.
        0x313D => {
            *value = 1;
            EGL_TRUE
        }
        _ => EGL_FALSE,
    }
}

// ---------------------------------------------------------------------------
// EGL sync objects and images
// ---------------------------------------------------------------------------
//
// `libhwui` imports these from `libEGL`, and none of them was exported by the
// gleshim — so Skia's fence handling called straight into Mesa with our synthetic
// surface and display handles. Mesa's first act in every entry point is
// `validate_display`, which is where the flood of
// "libEGL: validate_display:567 error 3008 (EGL_BAD_DISPLAY)" came from.
//
// A sync object here is a bare sentinel: creating one finishes the GPU work
// submitted so far (`eglCreateSyncKHR`), so every wait is already satisfied.

/// `EGL_CONDITION_SATISFIED_KHR`.
const EGL_CONDITION_SATISFIED_KHR: c_int = 0x30F6;

/// Distinct sentinels so a client that compares syncs sees different objects.
static SYNC_SENTINEL: u8 = 0;

/// `eglCreateSyncKHR(dpy, type, attrib_list)`.
#[no_mangle]
pub unsafe extern "C" fn eglCreateSyncKHR(
    _dpy: EGLDisplay,
    kind: u32,
    attrib_list: *const c_int,
) -> *mut c_void {
    const EGL_SYNC_NATIVE_FENCE_ANDROID: u32 = 0x3144;
    const EGL_SYNC_NATIVE_FENCE_FD_ANDROID: c_int = 0x3145;
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglCreateSyncKHR kind={kind:#x} thread={}", thread_label());
    }
    let imported = (kind == EGL_SYNC_NATIVE_FENCE_ANDROID)
        .then(|| read_attribs(attrib_list))
        .flatten()
        .and_then(|list| {
            list.chunks(2).find(|pair| pair.len() == 2 && pair[0] == EGL_SYNC_NATIVE_FENCE_FD_ANDROID)
                .map(|pair| pair[1])
        });
    match imported {
        // A consumer waiting on a producer's fence: the producer finished
        // its GPU work before handing the fd out (see below), and EGL takes
        // ownership of the fd.
        Some(fd) if fd >= 0 => { libc::close(fd); }
        // A producer asking for a fence over the work submitted so far, plain
        // (EGL_SYNC_FENCE_KHR) or native. Waits on our syncs return at once,
        // so the work has to be complete before the sync exists. Chromium
        // hands a WebGL frame from its GPU thread to the RenderThread
        // compositor (another share group, through an EGLImage) behind a
        // plain fence; without this finish the compositor could sample the
        // texture before the clear/draw landed, and a canvas that is never
        // redrawn stayed white.
        _ => { if let Some(finish) = host().ok().and_then(|h| h.gl_finish) { finish(); } }
    }
    std::ptr::addr_of!(SYNC_SENTINEL) as *mut c_void
}

/// `eglDestroySyncKHR(dpy, sync)`.
#[no_mangle]
pub unsafe extern "C" fn eglDestroySyncKHR(_dpy: EGLDisplay, _sync: *mut c_void) -> u32 {
    EGL_TRUE
}

/// `eglClientWaitSyncKHR(dpy, sync, flags, timeout)` — always already signalled.
#[no_mangle]
pub unsafe extern "C" fn eglClientWaitSyncKHR(
    _dpy: EGLDisplay,
    _sync: *mut c_void,
    _flags: c_int,
    _timeout: u64,
) -> c_int {
    EGL_CONDITION_SATISFIED_KHR
}

/// `eglWaitSyncKHR(dpy, sync, flags)`.
#[no_mangle]
pub unsafe extern "C" fn eglWaitSyncKHR(
    _dpy: EGLDisplay,
    _sync: *mut c_void,
    _flags: c_int,
) -> u32 {
    EGL_TRUE
}

/// `eglGetSyncAttribKHR(dpy, sync, attribute, value)`.
#[no_mangle]
pub unsafe extern "C" fn eglGetSyncAttribKHR(
    _dpy: EGLDisplay,
    _sync: *mut c_void,
    attribute: c_int,
    value: *mut c_int,
) -> u32 {
    if !value.is_null() {
        // EGL_SYNC_STATUS_KHR -> EGL_SIGNALED_KHR, EGL_SYNC_TYPE_KHR -> EGL_SYNC_FENCE_KHR.
        *value = match attribute {
            0x30F1 => 0x30F2, // EGL_SYNC_STATUS_KHR -> EGL_SIGNALED_KHR
            0x30F0 => 0x30F9, // EGL_SYNC_TYPE_KHR -> EGL_SYNC_FENCE_KHR
            _ => 0,
        };
    }
    EGL_TRUE
}

/// `eglDupNativeFenceFDANDROID(dpy, sync)`: an already-signalled fence fd.
///
/// The GPU work it guards was finished when the sync was created, so the fd
/// only has to read as signalled: an eventfd with a count of 1 is readable,
/// which is what `sync_wait`/`poll(POLLIN)` check. Returning
/// EGL_NO_NATIVE_FENCE_FD made Chromium drop every WebGL frame it hands to
/// the compositor ("Unable to get a gpu fence object").
#[no_mangle]
pub unsafe extern "C" fn eglDupNativeFenceFDANDROID(
    _dpy: EGLDisplay,
    _sync: *mut c_void,
) -> c_int {
    libc::eventfd(1, libc::EFD_CLOEXEC)
}

/// `eglPresentationTimeANDROID(dpy, surface, time)`.
///
/// Android's `libEGL` extension. `libandroid_runtime` imports it, and that was
/// the per-frame path that reached the Bionic `libEGL` instead of this shim.
/// A surface here is a synchronous offscreen FBO, so a presentation timestamp
/// has nothing to do; accepting it is the whole contract.
#[no_mangle]
pub unsafe extern "C" fn eglPresentationTimeANDROID(
    _dpy: EGLDisplay,
    surface: EGLSurface,
    _time: i64,
) -> u32 {
    if surface.is_null() {
        return EGL_FALSE;
    }
    EGL_TRUE
}

// ---------------------------------------------------------------------------
// Hosted physical display (NDK display API used by HWUI DeviceInfo)
// ---------------------------------------------------------------------------

/// PenguDroid has one virtual internal display. HWUI asks for the physical
/// display list while creating an EGL surface; forwarding that query through a
/// second SurfaceFlinger/binder runtime is unnecessary and, with Hybris on the
/// same RenderThread, corrupts the guest IPC state.
static PENGUDROID_DISPLAY: u8 = 0;

#[export_name = "_ZN7android32ADisplay_acquirePhysicalDisplaysEPPPNS_8ADisplayE"]
pub unsafe extern "C" fn ADisplay_acquirePhysicalDisplays(out: *mut *mut *mut c_void) -> c_int {
    if out.is_null() {
        return 0;
    }
    let displays = libc::malloc(std::mem::size_of::<*mut c_void>()) as *mut *mut c_void;
    if displays.is_null() {
        *out = std::ptr::null_mut();
        return 0;
    }
    *displays = std::ptr::addr_of!(PENGUDROID_DISPLAY) as *mut c_void;
    *out = displays;
    1
}

#[export_name = "_ZN7android16ADisplay_releaseEPPNS_8ADisplayE"]
pub unsafe extern "C" fn ADisplay_release(displays: *mut *mut c_void) {
    libc::free(displays as *mut c_void);
}

#[export_name = "_ZN7android23ADisplay_getDisplayTypeEPNS_8ADisplayE"]
pub unsafe extern "C" fn ADisplay_getDisplayType(display: *mut c_void) -> c_int {
    if display == std::ptr::addr_of!(PENGUDROID_DISPLAY) as *mut c_void {
        0 // DISPLAY_TYPE_INTERNAL
    } else {
        -1
    }
}

#[export_name = "_ZN7android36ADisplay_getPreferredWideColorFormatEPNS_8ADisplayEP10ADataSpaceP22AHardwareBuffer_Format"]
pub unsafe extern "C" fn ADisplay_getPreferredWideColorFormat(
    display: *mut c_void,
    dataspace: *mut c_int,
    pixel_format: *mut c_int,
) -> c_int {
    if display != std::ptr::addr_of!(PENGUDROID_DISPLAY) as *mut c_void {
        return -1;
    }
    if !dataspace.is_null() {
        *dataspace = 142_671_872; // ADATASPACE_SRGB
    }
    if !pixel_format.is_null() {
        *pixel_format = 1; // AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM
    }
    0
}

/// Resolve the guest binder::Status constructor for the AArch64 AIDL stub.
/// This is lazy because gleshim is preloaded before libbinder exists.
#[no_mangle]
pub extern "C" fn __pengudroid_status_ctor() -> *mut c_void {
    static CTOR: OnceLock<usize> = OnceLock::new();
    let address = *CTOR.get_or_init(|| {
        let name = b"_ZN7android6binder6StatusC1Eii\0";
        // SAFETY: static NUL-terminated symbol name; the caller is an AIDL
        // method, so guest libbinder is loaded by this point.
        (unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr().cast()) }) as usize
    });
    address as *mut c_void
}

// ---------------------------------------------------------------------------
// Bionic libEGL internals
// ---------------------------------------------------------------------------
//
// The Bionic `libGLESv1_CM`/`libGLESv2`/`libGLESv3` are thin trampolines: their
// `gl*` entry points dispatch through the connection that `libEGL` sets up. Three
// of them plus one `libandroid_runtime` hook are the *only* symbols the rest of
// the tree takes from the Bionic `libEGL`, and leaving them unexported meant
// that library stayed in the call path — where its own `EGLDisplay` registry
// rejects this shim's surfaceless display:
//
//     libEGL: validate_display:567 error 3008 (EGL_BAD_DISPLAY)
//
// GL work never actually goes through those trampolines here: `LD_DEBUG=bindings`
// shows every `gl*` binding resolving to the host's `libGLESv2.so.2`. So they only
// have to link and be callable without crashing.

/// Opaque stand-in for the Bionic `egl_connection_t`.
///
/// Non-null so a trampoline that stores it does not fault immediately; its
/// dispatch table is empty on purpose, because the host's GL library is what
/// actually gets called.
static EGL_CONNECTION: [u8; 256] = [0; 256];

/// `android::egl_get_connection()`.
#[export_name = "_ZN7android18egl_get_connectionEv"]
pub unsafe extern "C" fn egl_get_connection() -> *mut c_void {
    std::ptr::addr_of!(EGL_CONNECTION) as *mut c_void
}

/// `android::egl_get_init_count(void*)` — `libandroid_runtime` asks whether EGL
/// has been initialized for a display.
#[export_name = "_ZN7android18egl_get_init_countEPv"]
pub unsafe extern "C" fn egl_get_init_count(_dpy: *mut c_void) -> u32 {
    if _dpy.is_null() {
        0
    } else {
        1
    }
}

/// `android::egl_get_string_for_current_context(unsigned int)` — the GLESv1
/// trampoline's `glGetString` for extension strings.
#[export_name = "_ZN7android34egl_get_string_for_current_contextEj"]
#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn egl_get_string_for_current_context(_name: u32) -> *const c_char {
    std::ptr::null()
}

/// `android::egl_get_string_for_current_context(unsigned int, unsigned int)` —
/// the per-display overload. `GLConsumer`/`libandroid_runtime` use it to read a
/// single extension string, so both return the same (absent) value.
#[export_name = "_ZN7android34egl_get_string_for_current_contextEjj"]
pub unsafe extern "C" fn egl_get_string_for_current_context2(
    _dpy: u32,
    _name: u32,
) -> *const c_char {
    std::ptr::null()
}

/// `android::egl_get_num_extensions_for_current_context()` — `libhwui` sizes
/// its EGL extension table from this.
#[export_name = "_ZN7android42egl_get_num_extensions_for_current_contextEv"]
pub unsafe extern "C" fn egl_get_num_extensions_for_current_context() -> i32 {
    0
}

/// `android::eglQueryStringImplementationANDROIDImpl(void*, int)` — the
/// unfiltered `eglQueryString` used by `libhwui` when it wants the real
/// implementation's string rather than the sanitized one.
#[export_name = "_ZN7android39eglQueryStringImplementationANDROIDImplEPvi"]
pub unsafe extern "C" fn eglQueryStringImplementationANDROIDImpl(
    dpy: *mut c_void,
    name: c_int,
) -> *const c_char {
    eglQueryString(dpy, name)
}

/// `android::egl_set_cache_filename(const char*)` — `libhwui` points the
/// driver's blob cache at the app's cache directory during
/// `HardwareRenderer.setupPersistentGraphicsCache`.
///
/// The shim renders into a host-backed offscreen FBO chain, so there is no
/// guest driver cache to persist. Returning immediately is the whole
/// implementation; the alternative is forwarding to the *host* `libEGL`, whose
/// blob cache belongs to Mesa and must not be pointed at a guest path.
#[export_name = "_ZN7android22egl_set_cache_filenameEPKc"]
pub unsafe extern "C" fn egl_set_cache_filename(filename: *const c_char) {
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        let shown = if filename.is_null() {
            "<null>".to_string()
        } else {
            std::ffi::CStr::from_ptr(filename).to_string_lossy().into_owned()
        };
        log_once(format_args!("egl_set_cache_filename({shown}) ignored"));
    }
}

thread_local! {
    /// `gl_hooks_t*` installed by `libhwui` through `setGlThreadSpecific`.
    static GL_THREAD_SPECIFIC: std::cell::Cell<*const c_void> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

/// `android::setGlThreadSpecific(const gl_hooks_t*)` — `libhwui` installs its
/// hook table per thread during `Extensions::initialize()`, and reads it back
/// through `getGlThreadSpecific()` on every GL entry point.
#[export_name = "_ZN7android19setGlThreadSpecificEPKNS_10gl_hooks_tE"]
pub unsafe extern "C" fn setGlThreadSpecific(hooks: *const c_void) {
    GL_THREAD_SPECIFIC.with(|slot| slot.set(hooks));
}

/// `android::getGlThreadSpecific()` — the matching getter.
#[export_name = "_ZN7android18getGlThreadSpecificEv"]
pub unsafe extern "C" fn getGlThreadSpecific() -> *const c_void {
    GL_THREAD_SPECIFIC.with(|slot| slot.get())
}

/// `android::eglBeginFrameImpl(void*, void*)` — see [`eglBeginFrame`].
#[export_name = "_ZN7android17eglBeginFrameImplEPvS0_"]
pub unsafe extern "C" fn eglBeginFrameImpl(_dpy: *mut c_void, _surface: *mut c_void) {}

/// `eglGetPlatformDisplay(EGLenum, void*, const EGLAttrib*)` — the EGL 1.5
/// entry point `libhwui` prefers. Forwarded to the host's
/// `eglGetPlatformDisplayEXT`, which takes the same arguments.
#[no_mangle]
pub unsafe extern "C" fn eglGetPlatformDisplay(
    platform: u32,
    native_display: *mut c_void,
    attribs: *const isize,
) -> EGLDisplay {
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return std::ptr::null_mut();
        }
    };
    // `EGL_EXT_platform_base` declares `EGLint *` where EGL 1.5 has
    // `EGLAttrib *`, so the two lists are not layout-compatible and cannot be
    // passed through. The shim only supports the surfaceless platform, which
    // defines no attributes at all, so a non-empty list is dropped rather than
    // reinterpreted (a misread pointer-sized value would fault in Mesa).
    if !attribs.is_null() && *attribs != 0 {
        log_once(format_args!(
            "eglGetPlatformDisplay: dropping platform attributes (unsupported)"
        ));
    }
    // Prefer the real EGL 1.5 entry point obtained with dlsym. In particular,
    // Aurora's host eglGetProcAddress("eglGetPlatformDisplayEXT") may return
    // this shim's EXT wrapper; calling that wrapper from here recurses forever.
    if let Some(get_platform_display) = host.get_platform_display {
        let display = get_platform_display(platform, native_display, attribs);
        if !display.is_null() {
            lock_state().display = display as usize;
            return display;
        }
    }
    if let Some(get_platform_display) = host.get_platform_display_ext {
        if get_platform_display as usize == eglGetPlatformDisplayEXT as usize {
            return eglGetDisplay(native_display);
        }
        // SAFETY: the shim ignores the attribute list, so the empty list the
        // EXT signature expects is always the correct one here.
        let display = get_platform_display(platform, native_display, std::ptr::null());
        if !display.is_null() {
            lock_state().display = display as usize;
            return display;
        }
    }
    // SAFETY: `native_display` is whatever the caller passed.
    eglGetDisplay(native_display)
}

#[no_mangle]
pub unsafe extern "C" fn eglGetPlatformDisplayEXT(
    platform: u32,
    native_display: *mut c_void,
    _attribs: *const c_int,
) -> EGLDisplay {
    eglGetPlatformDisplay(platform, native_display, std::ptr::null())
}

/// `eglQueryAPI()` — EGL 1.5. The shim only ever binds OpenGL ES.
#[no_mangle]
pub unsafe extern "C" fn eglQueryAPI() -> u32 {
    0x30A0 // EGL_OPENGL_ES_API
}

/// `eglBeginFrame(EGLDisplay, EGLSurface)`.
///
/// `libhwui` calls this once per frame when the driver advertises
/// `EGL_ANDROID_frame_timestamps`/debug-marking support, and it was the *only*
/// per-frame symbol still resolving to the Bionic `libEGL`. That copy validates
/// the display against its own registry first, which is where the
/// `validate_display:567 error 3008` flood came from. There is nothing to begin:
/// the surface is a synchronous offscreen FBO.
#[export_name = "_Z13eglBeginFramePvS_"]
pub unsafe extern "C" fn eglBeginFrame(_dpy: EGLDisplay, _surface: EGLSurface) {}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| {
        let v = v.trim();
        v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
    })
}

fn mangohud_config_value(key: &str) -> Option<String> {
    let raw = std::env::var("MANGOHUD_CONFIG").ok()?;
    for item in raw.split(',') {
        let item = item.trim();
        if item == key {
            return Some(String::new());
        }
        if let Some((k, v)) = item.split_once('=') {
            if k.trim() == key {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn mangohud_enabled() -> bool {
    if mangohud_config_value("no_display").is_some() {
        return false;
    }
    env_truthy("PENGUDROID_FPS_METER")
        || env_truthy("MANGOHUD")
        || std::env::var_os("MANGOHUD_CONFIG").is_some_and(|v| !v.is_empty())
}

struct MangoHudStats {
    last_frame: std::time::Instant,
    window_start: std::time::Instant,
    frames: u32,
    fps: f32,
    last_ms: f32,
    history: [f32; 24],
    cursor: usize,
}

fn record_mangohud_sample(surface: usize) -> (u32, f32, [f32; 24], usize) {
    use std::time::Instant;
    static STATS: OnceLock<Mutex<std::collections::HashMap<usize, MangoHudStats>>> =
        OnceLock::new();
    let now = Instant::now();
    let mut map = STATS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > 64 {
        map.clear();
    }
    let entry = map.entry(surface).or_insert_with(|| MangoHudStats {
        last_frame: now,
        window_start: now,
        frames: 0,
        fps: 60.0,
        last_ms: 16.7,
        history: [16.7; 24],
        cursor: 0,
    });
    let dt_ms = (now.duration_since(entry.last_frame).as_secs_f32() * 1000.0).clamp(0.1, 250.0);
    entry.last_frame = now;
    entry.last_ms = dt_ms;
    entry.history[entry.cursor] = dt_ms;
    entry.cursor = (entry.cursor + 1) % entry.history.len();
    entry.frames += 1;
    let elapsed = now.duration_since(entry.window_start).as_secs_f32();
    if elapsed >= 0.5 {
        entry.fps = entry.frames as f32 / elapsed;
        entry.frames = 0;
        entry.window_start = now;
    } else if entry.fps <= 0.0 {
        entry.fps = 1000.0 / dt_ms;
    }
    (entry.fps.round().clamp(0.0, 9999.0) as u32, entry.last_ms, entry.history, entry.cursor)
}

fn glyph_3x5(ch: char) -> [u8; 5] {
    match ch.to_ascii_uppercase() {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'E' => [0b111, 0b100, 0b110, 0b100, 0b111],
        'F' => [0b111, 0b100, 0b110, 0b100, 0b100],
        'G' => [0b111, 0b100, 0b101, 0b101, 0b111],
        'H' => [0b101, 0b101, 0b111, 0b101, 0b101],
        'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'N' => [0b111, 0b101, 0b101, 0b101, 0b101],
        'O' => [0b111, 0b101, 0b101, 0b101, 0b111],
        'P' => [0b111, 0b101, 0b111, 0b100, 0b100],
        'S' => [0b111, 0b100, 0b111, 0b001, 0b111],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        '.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        ':' => [0b000, 0b010, 0b000, 0b010, 0b000],
        _ => [0; 5],
    }
}

/// Render the most recent touch as a ring and crosshair into the surface FBO.
/// This deliberately uses the same state-preserving glScissor/glClear approach
/// as the performance HUD, so no Android shader, texture or VAO state is changed.
unsafe fn draw_touch_debug_overlay(surface: EGLSurface, host: &Host) {
    if std::env::var_os("PENGUDROID_TOUCH_DEBUG").is_none() {
        return;
    }
    let state = *TOUCH_DEBUG_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.last_ms == 0 || input_trace_now_ms().saturating_sub(state.last_ms) > 750 {
        return;
    }
    let rec = &*(surface as *const SurfaceRec);
    let (w, h) = (rec.w as i32, rec.h as i32);
    if w <= 0 || h <= 0 {
        return;
    }
    let (
        Some(enable), Some(disable), Some(is_enabled), Some(scissor),
        Some(clear_color), Some(clear), Some(get_int), Some(get_float), Some(get_bool),
        Some(color_mask),
    ) = (
        host.gl_enable, host.gl_disable, host.gl_is_enabled, host.gl_scissor,
        host.gl_clear_color, host.gl_clear, host.gl_get_integerv,
        host.gl_get_floatv, host.gl_get_booleanv, host.gl_color_mask,
    ) else {
        return;
    };
    let was_scissor = is_enabled(GL_SCISSOR_TEST) != 0;
    let mut prev_box = [0i32; 4];
    get_int(GL_SCISSOR_BOX, prev_box.as_mut_ptr());
    let mut prev_clear = [0f32; 4];
    get_float(GL_COLOR_CLEAR_VALUE, prev_clear.as_mut_ptr());
    let mut prev_mask = [1u8; 4];
    get_bool(GL_COLOR_WRITEMASK, prev_mask.as_mut_ptr());
    enable(GL_SCISSOR_TEST);
    color_mask(1, 1, 1, 1);
    let flipped = std::env::var_os("PENGUDROID_NO_PRESENT_FLIP").is_none();
    let (r, g, b) = if state.pressed { (1.0, 0.45, 0.08) } else { (0.15, 0.85, 1.0) };
    clear_color(r, g, b, 1.0);
    let cx = state.x.round() as i32;
    let cy = state.y.round() as i32;
    let radius = if state.pressed { 42 } else { 30 };
    let pixel = 4;
    let mut fill = |x: i32, y: i32, rw: i32, rh: i32| {
        if rw <= 0 || rh <= 0 || x < 0 || y < 0 || x + rw > w || y + rh > h {
            return;
        }
        let gl_y = if flipped { h - y - rh } else { y };
        scissor(x, gl_y, rw, rh);
        clear(GL_COLOR_BUFFER_BIT);
    };
    for dy in (-radius..=radius).step_by(pixel as usize) {
        for dx in (-radius..=radius).step_by(pixel as usize) {
            let d2 = dx * dx + dy * dy;
            if d2 <= radius * radius && d2 >= (radius - pixel) * (radius - pixel) {
                fill(cx + dx, cy + dy, pixel, pixel);
            }
        }
    }
    fill(cx - 2, cy - 14, 4, 28);
    fill(cx - 14, cy - 2, 28, 4);
    color_mask(prev_mask[0], prev_mask[1], prev_mask[2], prev_mask[3]);
    clear_color(prev_clear[0], prev_clear[1], prev_clear[2], prev_clear[3]);
    scissor(prev_box[0], prev_box[1], prev_box[2], prev_box[3]);
    if !was_scissor {
        disable(GL_SCISSOR_TEST);
    }
}

/// Render an in-frame MangoHud-style performance overlay into the surface FBO.
///
/// Uses `glScissor` + `glClear` so it works identically on GLES 1/2/3 without
/// disturbing shader programs, vertex arrays, or texture bindings.
unsafe fn draw_mangohud_overlay(surface: EGLSurface, host: &Host) {
    if !mangohud_enabled() {
        return;
    }
    let rec = &*(surface as *const SurfaceRec);
    if rec.w < 240 || rec.h < 120 {
        return;
    }
    let (
        Some(enable),
        Some(disable),
        Some(is_enabled),
        Some(scissor),
        Some(clear_color),
        Some(clear),
        Some(get_int),
        Some(get_float),
        Some(get_bool),
        Some(color_mask),
    ) = (
        host.gl_enable,
        host.gl_disable,
        host.gl_is_enabled,
        host.gl_scissor,
        host.gl_clear_color,
        host.gl_clear,
        host.gl_get_integerv,
        host.gl_get_floatv,
        host.gl_get_booleanv,
        host.gl_color_mask,
    )
    else {
        return;
    };

    let (fps, ms, history, cursor) = record_mangohud_sample(surface as usize);
    let was_scissor = is_enabled(GL_SCISSOR_TEST) != 0;
    let mut prev_box = [0i32; 4];
    get_int(GL_SCISSOR_BOX, prev_box.as_mut_ptr());
    let mut prev_clear = [0f32; 4];
    get_float(GL_COLOR_CLEAR_VALUE, prev_clear.as_mut_ptr());
    let mut prev_mask = [1u8; 4];
    get_bool(GL_COLOR_WRITEMASK, prev_mask.as_mut_ptr());

    enable(GL_SCISSOR_TEST);
    color_mask(1, 1, 1, 1);

    let w = rec.w as i32;
    let h = rec.h as i32;
    let flipped = std::env::var_os("PENGUDROID_NO_PRESENT_FLIP").is_none();
    let scale: i32 = if rec.w >= 1600 { 3 } else { 2 };
    let line1 = format!("MANGOHUD {fps:3} FPS");
    let line2 = format!("MESA {ms:5.1} MS");
    let chars = line1.len().max(line2.len()) as i32;
    let pad = 4 * scale;
    let char_step = 4 * scale;
    let box_w = (chars * char_step + pad * 2).min(w - 16);
    let graph_h = 8 * scale;
    let box_h = (5 * scale) * 2 + 4 * scale + graph_h + pad * 2;
    let pos = mangohud_config_value("position").unwrap_or_default();
    let margin = 12;
    let box_x = if pos.contains("right") { (w - box_w - margin).max(4) } else { margin };
    let box_y = if pos.contains("bottom") { (h - box_h - margin).max(4) } else { margin };

    let fill_rect = |sx: i32, sy: i32, rw: i32, rh: i32, r: f32, g: f32, b: f32| {
        if rw <= 0 || rh <= 0 {
            return;
        }
        let gl_y = if flipped { h - sy - rh } else { sy };
        scissor(sx.clamp(0, w), gl_y.clamp(0, h), rw, rh);
        clear_color(r, g, b, 1.0);
        clear(GL_COLOR_BUFFER_BIT);
    };

    let draw_text = |text: &str, tx: i32, ty: i32, r: f32, g: f32, b: f32| {
        clear_color(r, g, b, 1.0);
        for (idx, ch) in text.chars().enumerate() {
            let rows = glyph_3x5(ch);
            let gx = tx + idx as i32 * char_step;
            for (ry, bits) in rows.iter().enumerate() {
                for rx in 0..3i32 {
                    if (bits >> (2 - rx)) & 1 != 0 {
                        let px = gx + rx * scale;
                        let py = ty + ry as i32 * scale;
                        let gl_y = if flipped { h - py - scale } else { py };
                        scissor(px.clamp(0, w), gl_y.clamp(0, h), scale, scale);
                        clear(GL_COLOR_BUFFER_BIT);
                    }
                }
            }
        }
    };

    // Panel backdrop + left MangoHud orange accent strip
    fill_rect(box_x, box_y, box_w, box_h, 0.06, 0.07, 0.09);
    fill_rect(box_x, box_y, scale + 1, box_h, 0.96, 0.45, 0.12);

    let (fps_r, fps_g, fps_b) = if fps >= 50 {
        (0.45, 0.95, 0.45)
    } else if fps >= 30 {
        (0.98, 0.78, 0.25)
    } else {
        (0.96, 0.30, 0.30)
    };
    draw_text(&line1, box_x + pad, box_y + pad, fps_r, fps_g, fps_b);
    draw_text(
        &line2,
        box_x + pad,
        box_y + pad + 7 * scale,
        0.55,
        0.86,
        0.98,
    );

    // Mini frametime graph along bottom of HUD card
    let gx0 = box_x + pad;
    let gy0 = box_y + box_h - pad - graph_h;
    let bar_w = ((box_w - pad * 2) / history.len() as i32).max(2);
    for i in 0..history.len() {
        let sample = history[(cursor + i) % history.len()];
        let bh = ((sample / 33.3) * graph_h as f32).round().clamp(1.0, graph_h as f32) as i32;
        let (br, bg, bb) = if sample <= 18.5 {
            (0.35, 0.85, 0.45)
        } else if sample <= 34.0 {
            (0.95, 0.72, 0.22)
        } else {
            (0.92, 0.28, 0.28)
        };
        fill_rect(
            gx0 + i as i32 * bar_w,
            gy0 + (graph_h - bh),
            (bar_w - 1).max(1),
            bh,
            br,
            bg,
            bb,
        );
    }

    color_mask(prev_mask[0], prev_mask[1], prev_mask[2], prev_mask[3]);
    clear_color(prev_clear[0], prev_clear[1], prev_clear[2], prev_clear[3]);
    scissor(prev_box[0], prev_box[1], prev_box[2], prev_box[3]);
    if !was_scissor {
        disable(GL_SCISSOR_TEST);
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglSwapBuffers(_dpy: EGLDisplay, surface: EGLSurface) -> u32 {
    let profile_started = std::time::Instant::now();
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        static SWAPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SWAPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n == 0 {
            eprintln!(
                "[gleshim] swap mode FORCE_SHM={:?} FORCE_DMABUF={:?}",
                std::env::var_os("PENGUDROID_FORCE_SHM"),
                std::env::var_os("PENGUDROID_FORCE_DMABUF"),
            );
        }
        if n < 5 || n % 60 == 0 {
            eprintln!("[gleshim] eglSwapBuffers #{n} surface={surface:p}");
        }
    }
    if surface.is_null() {
        return EGL_FALSE;
    }
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return EGL_FALSE;
        }
    };

    if let Err(e) = ensure_host_context(host) {
        log_once(format_args!("eglSwapBuffers: {e}"));
        return EGL_FALSE;
    }
    // Bind this surface's FBO: `ensure_fbo` alone leaves whatever the last
    // `eglMakeCurrent` bound, and two windows share one render thread.
    if let Err(e) = bind_surface_fbo(surface, host) {
        log_once(format_args!("eglSwapBuffers: {e}"));
        return EGL_FALSE;
    }
    if std::env::var_os("PENGUDROID_FORCE_DMABUF").is_none() {
        gralloc::probe_gralloc_once();
    }
    trace_touch_swap();
    draw_mangohud_overlay(surface, host);
    draw_touch_debug_overlay(surface, host);
    // Requesting gralloc is not enough: use it only after this surface's
    // allocation succeeded. Otherwise the same binary continues with the
    // active Linux/Hybris driver's dma-buf export path.
    let gralloc_active = std::env::var_os("PENGUDROID_FORCE_DMABUF").is_none()
        && gralloc::has_target(surface, host);
    let async_gralloc = gralloc_active
        && gralloc::async_requested()
        && std::env::var_os("PENGUDROID_GRALLOC_SHM").is_some();
    if async_gralloc {
        gralloc::wait_gralloc_async_slot(surface);
    }
    let finish_started = std::time::Instant::now();
    // Diagnostic readback still requires synchronous source pixels. Normal
    // async swaps instead use a real host fence after the output flip.
    if !async_gralloc
        || std::env::var_os("PENGUDROID_TRACE_CONTENT").is_some()
        || std::env::var_os("PENGUDROID_DUMP_FRAME").is_some()
    {
        if let Some(finish) = host.gl_finish {
            finish();
        }
    }
    let finish_elapsed = finish_started.elapsed();
    trace_content_hash(surface, host);
    let flip_started = std::time::Instant::now();
    gralloc::flip_gralloc_surface(surface, host);
    let flip_elapsed = flip_started.elapsed();
    dump_frame_once(surface, host);
    trace_presents(surface);

    if gralloc_active && std::env::var_os("PENGUDROID_GRALLOC_SHM").is_some() {
        let present_started = std::time::Instant::now();
        let presented = if async_gralloc {
            match gralloc::queue_gralloc_async(surface, host) {
                Ok(()) => Ok(()),
                Err(error) => {
                    eprintln!("[gleshim:gralloc] async queue failed, using synchronous fence fallback: {error}");
                    if let Some(finish) = host.gl_finish { finish(); }
                    gralloc::present_gralloc(surface)
                }
            }
        } else {
            gralloc::present_gralloc(surface)
        };
        let present_elapsed = present_started.elapsed();
        let gralloc_presented = presented.is_ok();
        let pace_started = std::time::Instant::now();
        if presented.is_ok() {
            gralloc::pace_gralloc_swap(surface);
        }
        let pace_elapsed = pace_started.elapsed();
        let outcome = match presented {
            Ok(()) => EGL_TRUE,
            Err(error) if !async_gralloc => {
                static LOGGED_SHM_FALLBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !LOGGED_SHM_FALLBACK.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("[gleshim:gralloc] wl_shm present failed, using readback fallback: {error}");
                }
                match present_shm(surface) {
                    Ok(()) => EGL_TRUE,
                    Err(fallback) => {
                        eprintln!("[gleshim:gralloc] readback fallback failed: {fallback}");
                        EGL_FALSE
                    }
                }
            }
            Err(error) => {
                eprintln!("[gleshim:gralloc] async synchronous fallback failed: {error}");
                EGL_FALSE
            }
        };
        if !async_gralloc {
            trace_touch_present(
                if gralloc_presented { "gralloc-shm" } else { "gralloc-readback" },
                outcome == EGL_TRUE, finish_elapsed, flip_elapsed, present_elapsed,
            );
            if outcome == EGL_TRUE {
                notify_frame_presented_vsync();
            }
        }
        if std::env::var_os("PENGUDROID_PROFILE_GRALLOC").is_some() {
            gralloc::profile_gralloc(
                finish_elapsed, flip_elapsed, present_elapsed, pace_elapsed,
                profile_started.elapsed(),
            );
        }
        return outcome;
    }

    let dmabuf_supported = dmabuf::is_supported(host);
    if (std::env::var_os("PENGUDROID_FORCE_SHM").is_some()
        && std::env::var_os("PENGUDROID_FORCE_DMABUF").is_none())
        || !dmabuf_supported
    {
        if !dmabuf_supported {
            log_once(format_args!(
                "active host EGL has no complete dma-buf export API; using SHM"
            ));
        }
        return match present_shm(surface) {
            Ok(()) => {
                pace_swap(surface as usize);
                notify_frame_presented_vsync();
                EGL_TRUE
            }
            Err(e) => {
                log_once(format_args!("eglSwapBuffers SHM: {e}"));
                pace_swap(surface as usize);
                EGL_TRUE
            }
        };
    }

    let exported = match export_surface_dmabuf(surface) {
        Ok(exported) => {
            static LOGGED_EXPORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !LOGGED_EXPORT.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "[gleshim] dma-buf export {}x{} stride={} fourcc={:#x} modifier={:#x}",
                    exported.width, exported.height, exported.stride, exported.fourcc, exported.modifier
                );
            }
            exported
        }
        Err(e) => {
            static LOGGED_EXPORT_ERROR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !LOGGED_EXPORT_ERROR.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("[gleshim] dma-buf export failed, using SHM: {e}");
            }
            return match present_shm(surface) {
                Ok(()) => {
                    pace_swap(surface as usize);
                    notify_frame_presented_vsync();
                    EGL_TRUE
                }
                Err(shm_error) => {
                    eprintln!("[gleshim] SHM fallback failed: {shm_error}");
                    EGL_FALSE
                }
            };
        }
    };
    let rec = &*(surface as *const SurfaceRec);

    // Take ownership so the fd is closed no matter how presentation ends; the
    // daemon receives its own descriptor via SCM_RIGHTS.
    // SAFETY: `exported.fd` is a freshly exported descriptor we own.
    let fd = OwnedFd::from_raw_fd(exported.fd);

    match dmabuf::present_dmabuf(
        surface,
        rec.w,
        rec.h,
        exported.stride,
        exported.fourcc,
        exported.modifier,
        exported.offset,
        fd.as_raw_fd(),
    ) {
        Ok(()) => {
            pace_swap(surface as usize);
            notify_frame_presented_vsync();
            EGL_TRUE
        }
        Err(e) => {
            log_once(format_args!("eglSwapBuffers: {e}"));
            if std::env::var_os("PENGUDROID_GRALLOC").is_some() {
                static LOGGED_GRALLOC_FALLBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !LOGGED_GRALLOC_FALLBACK.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("[gleshim:gralloc] dma-buf present failed, using SHM fallback: {e}");
                }
                if let Err(shm_error) = present_shm(surface) {
                    eprintln!("[gleshim:gralloc] SHM fallback failed: {shm_error}");
                }
            }
            // Transient daemon timeouts/reconnects must not make SDL3/hwui tear
            // down the entire rendering context; next swap will reconnect.
            pace_swap(surface as usize);
            EGL_TRUE
        }
    }
}

/// Minimum interval between two swaps, in microseconds, for a given cap.
/// `0` means "no cap" (the historical behaviour).
fn frame_gap_micros(max_fps: u64) -> Option<u64> {
    if max_fps == 0 {
        None
    } else {
        Some(1_000_000u64.div_ceil(max_fps))
    }
}

/// How long to sleep so that `now + sleep` lands on `last + gap`. A swap that
/// already arrived late (rendering took longer than the gap) sleeps zero, and
/// an overflowing `last + gap` is treated as "due" rather than sleeping forever.
fn micros_to_sleep(last_us: u64, now_us: u64, gap_us: u64) -> u64 {
    match last_us.checked_add(gap_us) {
        Some(next) => next.saturating_sub(now_us),
        None => 0,
    }
}

/// Reserve the next frame slot before releasing the map lock. Concurrent
/// RenderThreads must not both compute a sleep from the same last-call time:
/// doing so lets their wakeups interleave inside one vsync interval.
fn reserve_frame_slot(previous_next: Option<u64>, now_us: u64, gap_us: u64) -> (u64, u64) {
    let scheduled = previous_next.map(|next| next.max(now_us)).unwrap_or(now_us);
    let next = scheduled.checked_add(gap_us).unwrap_or(now_us);
    (scheduled.saturating_sub(now_us), next)
}

/// Pace all Android EGL windows attached to one Wayland surface together.
/// HWUI can replace its EGLSurface during navigation; keying the limiter by
/// that short-lived pointer let their frame budgets add up and produced a
/// nominal 70 Hz stream with a 60 Hz cap. The daemon surface id is stable and
/// is also the object on which the visual frame counter is reported.
/// `present_dmabuf` is synchronous — it waits for the daemon's ack — but the
/// daemon acks as soon as the frame is *queued* for the compositor, not when
/// the compositor presents it. Without a cap a game free-runs at hundreds of
/// fps and burns a core rendering frames the daemon then drops
/// (`frame_slot_available` keeps only 3 pending buffers). Sleeping the
/// remainder of the frame budget here paces the guest to `PENGUDROID_MAX_FPS`
/// (default 60, `0` disables). This is deliberately guest-side: blocking the
/// daemon on a wl_callback would stall every other client, including input.
///
/// The budget is per surface. One process-wide clock made every surface share
/// one 60 Hz budget, so an app with an hwui window and a game surface (Beach
/// Buggy Racing 2 has four) slowed each of them down.
fn pace_swap(surface: usize) {
    use std::time::{Duration, Instant};
    static LAST_US: OnceLock<Mutex<std::collections::HashMap<usize, u64>>> = OnceLock::new();
    static MAX_FPS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let max_fps = *MAX_FPS.get_or_init(|| {
        std::env::var("PENGUDROID_MAX_FPS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .or_else(|| {
                mangohud_config_value("fps_limit").and_then(|v| v.parse().ok())
            })
            .unwrap_or(60)
    });
    let Some(gap) = frame_gap_micros(max_fps) else { return };
    // `Instant` has no absolute epoch; anchor our own clock at the first call
    // so the arithmetic stays in u64 microseconds.
    static ANCHOR: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let anchor = *ANCHOR.get_or_init(Instant::now);
    let now_us = anchor.elapsed().as_micros() as u64;
    let sleep = {
        let mut last = LAST_US.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        // A destroyed surface's address can be reused; a stale entry only
        // costs at most one frame budget, and the map stays as small as the
        // number of surfaces ever swapped.
        if last.len() > 256 { last.clear(); }
        let (sleep, next) = reserve_frame_slot(last.get(&surface).copied(), now_us, gap);
        last.insert(surface, next);
        sleep
    };
    if sleep > 0 {
        std::thread::sleep(Duration::from_micros(sleep));
    }
}

/// `PENGUDROID_DUMP_FRAME=/path/frame.ppm`: write the first few frames to disk.
///
/// The compositor cannot be screenshotted from a sandboxed session, so this is
/// how the render output is inspected: the FBO is read back with
/// `glReadPixels` and written as a binary PPM (no image crate needed).
/// `PENGUDROID_TRACE_PRESENTS`: one line per surface per second with how many
/// frames it swapped, so a window that stops redrawing is visible.
fn trace_presents(surface: EGLSurface) {
    if std::env::var_os("PENGUDROID_TRACE_PRESENTS").is_none() {
        return;
    }
    static COUNTS: Mutex<Vec<(usize, u64, Option<std::time::Instant>)>> = Mutex::new(Vec::new());
    let mut counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let key = surface as usize;
    let i = match counts.iter().position(|(s, _, _)| *s == key) {
        Some(i) => i,
        None => { counts.push((key, 0, None)); counts.len() - 1 }
    };
    let entry = &mut counts[i];
    entry.1 += 1;
    if entry.2.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1)) {
        // SAFETY: every non-null surface is a SurfaceRec this module allocated.
        let rec = unsafe { &*(surface as *const SurfaceRec) };
        eprintln!("[gleshim] presents surface {} ({}x{}) total {}", rec.surface_id, rec.w, rec.h, entry.1);
        entry.2 = Some(std::time::Instant::now());
    }
}

unsafe fn dump_frame_once(surface: EGLSurface, host: &Host) {
    let Some(path) = std::env::var_os("PENGUDROID_DUMP_FRAME") else {
        return;
    };
    static DUMPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Skip the first frames: the first traversal draws the window background
    // before the app has laid anything out. `PENGUDROID_DUMP_FRAME_AT` overrides
    // the frame number, which matters for an app that only swaps a handful of
    // times (a static screen) and would never reach the default.
    let at: u64 = std::env::var("PENGUDROID_DUMP_FRAME_AT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(200);
    // `PENGUDROID_DUMP_FRAME_EVERY=<n>` writes every n-th frame instead of one
    // specific frame. A playing video invalidates continuously, so a sequence of
    // frames is the only way to tell "the video is on screen" from "the last
    // drawn frame happened to be the video".
    let every: u64 = std::env::var("PENGUDROID_DUMP_FRAME_EVERY")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let n = DUMPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let wanted = if every > 0 {
        n > 0 && n % every == 0
    } else {
        n == at
    };
    if !wanted {
        return;
    }
    let Some(read_pixels) = host.gl_read_pixels else {
        return;
    };
    let rec = &*(surface as *const SurfaceRec);
    let (w, h) = (rec.w as usize, rec.h as usize);
    let mut rgba = vec![0u8; w * h * 4];
    read_pixels(
        0,
        0,
        w as i32,
        h as i32,
        0x1908, // GL_RGBA
        0x1401, // GL_UNSIGNED_BYTE
        rgba.as_mut_ptr() as *mut c_void,
    );
    let mut ppm = Vec::with_capacity(w * h * 3 + 32);
    ppm.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    // GL's origin is bottom-left; PPM's is top-left. `PENGUDROID_DUMP_FRAME_NOFLIP`
    // writes the rows in readback order instead, which is exactly what the
    // dma-buf handed to the compositor contains — so the two images together show
    // whether presentation needs a flip.
    let y_range: Vec<usize> = if std::env::var_os("PENGUDROID_DUMP_FRAME_NOFLIP").is_some() {
        (0..h).collect()
    } else {
        (0..h).rev().collect()
    };
    for y in y_range {
        for x in 0..w {
            let i = (y * w + x) * 4;
            ppm.extend_from_slice(&rgba[i..i + 3]);
        }
    }
    let path = std::path::PathBuf::from(path);
    // The surface id is part of the name because two windows render at once and
    // the frame counter is global.
    let path = path.with_extension(format!("s{}frame{n}.ppm", rec.surface_id));
    match std::fs::write(&path, &ppm) {
        Ok(()) => eprintln!("[gleshim] wrote {} ({w}x{h})", path.display()),
        Err(e) => eprintln!("[gleshim] cannot write {}: {e}", path.display()),
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglReleaseThread() -> u32 {
    EGL_TRUE
}

// ---------------------------------------------------------------------------
// Exported GLESv2 API (a small forwarding surface)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn glClear(mask: u32) {
    if let Some(f) = host().ok().and_then(|h| h.gl_clear) {
        // SAFETY: forwarded verbatim.
        f(mask);
    }
}

#[cfg_attr(not(target_arch = "arm"), no_mangle)]
#[cfg_attr(target_arch = "arm", export_name = "__pengudroid_rust_glClearColor")]
pub unsafe extern "C" fn glClearColor(red: f32, green: f32, blue: f32, alpha: f32) {
    if let Some(f) = host().ok().and_then(|h| h.gl_clear_color) {
        // SAFETY: forwarded verbatim.
        f(red, green, blue, alpha);
    }
}

#[no_mangle]
pub unsafe extern "C" fn glViewport(x: c_int, y: c_int, width: c_int, height: c_int) {
    if let Some(f) = host().ok().and_then(|h| h.gl_viewport) {
        // SAFETY: forwarded verbatim.
        f(x, y, width, height);
    }
}

#[no_mangle]
pub unsafe extern "C" fn glFinish() {
    if let Some(f) = host().ok().and_then(|h| h.gl_finish) {
        // SAFETY: forwarded verbatim.
        f();
    }
}

#[no_mangle]
pub unsafe extern "C" fn glFlush() {
    if let Some(f) = host().ok().and_then(|h| h.gl_flush) {
        // SAFETY: forwarded verbatim.
        f();
    }
}

#[no_mangle]
pub unsafe extern "C" fn glGetError() -> u32 {
    match host().ok().and_then(|h| h.gl_get_error) {
        // SAFETY: forwarded verbatim.
        Some(f) => f(),
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn glGetString(name: u32) -> *const u8 {
    match host().ok().and_then(|h| h.gl_get_string) {
        // SAFETY: forwarded verbatim.
        Some(f) => f(name),
        None => std::ptr::null(),
    }
}

/// `glBindFramebuffer(target, framebuffer)`.
///
/// hwui's Skia backend renders into the *default* framebuffer (id 0) and binds
/// it explicitly before every flush. A surfaceless host context has no default
/// framebuffer, so binding 0 is redirected to the current surface's FBO — that
/// is what makes the app's pixels land somewhere we can export.
#[no_mangle]
pub unsafe extern "C" fn glBindFramebuffer(target: u32, framebuffer: u32) {
    let Ok(host) = host() else {
        return;
    };
    let Some(bind) = host.gl_bind_framebuffer else {
        return;
    };
    if framebuffer == 0 {
        let surface = current_surface() as EGLSurface;
        if !surface.is_null() {
            if let Ok((_, fbo)) = ensure_fbo(surface, host) {
                bind(target, fbo);
                return;
            }
        }
    }
    bind(target, framebuffer);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_extension_list_gains_shim_extensions_once() {
        let out = extend_extension_list("EGL_KHR_fence_sync EGL_KHR_swap_buffers_with_damage");
        let out = out.to_str().unwrap();
        assert!(out.starts_with("EGL_KHR_fence_sync "));
        assert_eq!(out.matches("EGL_KHR_swap_buffers_with_damage").count(), 1);
        assert!(out.contains("EGL_ANDROID_image_native_buffer"));
        assert_eq!(extend_extension_list("").to_str().unwrap().split(' ').next(), Some("EGL_KHR_swap_buffers_with_damage"));
    }

    use super::*;

    #[test]
    fn frame_pacing_math() {
        // 0 disables the cap entirely.
        assert_eq!(frame_gap_micros(0), None);
        // 60 fps -> 16667 us (rounded up, never shorter than the frame budget).
        assert_eq!(frame_gap_micros(60), Some(16_667));
        assert_eq!(frame_gap_micros(1), Some(1_000_000));
        // On time or late swap: nothing to sleep.
        assert_eq!(micros_to_sleep(1_000, 1_000, 16_667), 16_667);
        assert_eq!(micros_to_sleep(1_000, 17_667, 16_667), 0);
        assert_eq!(micros_to_sleep(1_000, 50_000, 16_667), 0);
        // Early swap: sleep exactly the remainder.
        assert_eq!(micros_to_sleep(1_000, 2_000, 16_667), 15_667);
        // u64 underflow must not panic on a huge `last`.
        assert_eq!(micros_to_sleep(u64::MAX, 0, 10), 0);
        // Concurrent callers reserve different slots instead of both sleeping
        // against the same timestamp; an idle producer remains immediate.
        assert_eq!(reserve_frame_slot(None, 10_000, 16_667), (0, 26_667));
        assert_eq!(reserve_frame_slot(Some(26_667), 11_000, 16_667), (15_667, 43_334));
        assert_eq!(reserve_frame_slot(Some(26_667), 50_000, 16_667), (0, 66_667));
    }

    #[test]
    fn read_attribs_stops_at_egl_none() {
        let list = [0x3033, 0x0004, 0x3040, 0x0004, EGL_NONE];
        let got = unsafe { read_attribs(list.as_ptr()) }.expect("terminated list");
        assert_eq!(got, list);
        assert!(unsafe { read_attribs(std::ptr::null()) }.is_none());
    }

    /// An unterminated list must be rejected instead of scanned forever. This
    /// was a real guest-visible hang: the render thread spun in this loop until
    /// the process was killed.
    #[test]
    fn read_attribs_rejects_unterminated_list() {
        // Any key other than EGL_NONE: the point is that none appears.
        let list = [0x3098 as c_int; MAX_ATTRIB_PAIRS * 4];
        assert!(
            unsafe { read_attribs(list.as_ptr()) }.is_none(),
            "a list without EGL_NONE must not be accepted"
        );
    }

    /// Resolve a GL entry point through the shim's own `eglGetProcAddress`.
    unsafe fn gl_proc<T: Copy>(name: &str) -> Option<T> {
        let c_name = CString::new(name).ok()?;
        let ptr = eglGetProcAddress(c_name.as_ptr());
        if ptr.is_null() {
            return None;
        }
        Some(std::mem::transmute_copy(&ptr))
    }

    #[test]
    fn linux_dma_buf_does_not_require_gralloc() {
        let host = host().expect("host EGL must load");
        assert!(dmabuf::is_supported(host));
        assert!(gralloc::support(host).is_err());
    }

    #[test]
    fn host_egl_loads() {
        match host() {
            Ok(host) => {
                assert!(
                    host.get_proc_address.is_some(),
                    "host eglGetProcAddress did not resolve"
                );
                assert!(
                    host.create_image_khr.is_some(),
                    "host eglCreateImageKHR did not resolve"
                );
                assert!(
                    host.export_dmabuf_query.is_some(),
                    "host eglExportDMABUFImageQueryMESA did not resolve"
                );
                assert!(host.gl_clear.is_some(), "host glClear did not resolve");
                assert!(host.gl_finish.is_some(), "host glFinish did not resolve");
            }
            Err(e) => eprintln!("skip host_egl_loads: {e}"),
        }
    }

    #[test]
    fn create_context_and_swap_geometry() {
        if let Err(e) = host() {
            eprintln!("skip create_context_and_swap_geometry: {e}");
            return;
        }

        // This test must exercise the no-daemon path.
        std::env::remove_var("PENGUDROID_SURFACE_ID");
        std::env::set_var("PENGUDROID_SURFACE_W", "64");
        std::env::set_var("PENGUDROID_SURFACE_H", "48");

        unsafe {
            let dpy = eglGetDisplay(std::ptr::null_mut());
            if dpy.is_null() {
                eprintln!("skip create_context_and_swap_geometry: no EGL display");
                return;
            }
            let mut major: c_int = 0;
            let mut minor: c_int = 0;
            if eglInitialize(dpy, &mut major, &mut minor) != EGL_TRUE {
                eprintln!("skip create_context_and_swap_geometry: eglInitialize failed");
                return;
            }
            assert_eq!(eglBindAPI(0x30A0), EGL_TRUE, "eglBindAPI(ES) failed");

            let config_attribs: [c_int; 13] = [
                0x3033, 0x0001, // EGL_SURFACE_TYPE, EGL_PBUFFER_BIT
                0x3040, 0x0004, // EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT
                0x3024, 8, // EGL_RED_SIZE
                0x3023, 8, // EGL_GREEN_SIZE
                0x3022, 8, // EGL_BLUE_SIZE
                0x3021, 8, // EGL_ALPHA_SIZE
                0x3038, // EGL_NONE
            ];
            let mut config: EGLConfig = std::ptr::null_mut();
            let mut num_configs: c_int = 0;
            assert_eq!(
                eglChooseConfig(
                    dpy,
                    config_attribs.as_ptr(),
                    &mut config,
                    1,
                    &mut num_configs,
                ),
                EGL_TRUE,
                "eglChooseConfig failed"
            );
            assert!(num_configs >= 1 && !config.is_null(), "no config returned");

            let invalid_attribs = [0x3098, 2, 0x7fff_ffff, 1, 0x3038];
            assert!(eglCreateContext(dpy, config, std::ptr::null_mut(), invalid_attribs.as_ptr()).is_null());
            assert_eq!(eglGetError(), 0x3004, "unknown attributes must still be rejected");
            let context_attribs = [0x3098, 2, 0x3570, 1, 0x3038]; // HWUI's Android telemetry hint
            let ctx = eglCreateContext(
                dpy,
                config,
                std::ptr::null_mut(),
                context_attribs.as_ptr(),
            );
            assert!(!ctx.is_null(), "eglCreateContext failed");

            let surface =
                eglCreateWindowSurface(dpy, config, std::ptr::null_mut(), std::ptr::null());
            assert!(!surface.is_null(), "eglCreateWindowSurface failed");

            assert_eq!(
                eglMakeCurrent(dpy, surface, surface, ctx),
                EGL_TRUE,
                "eglMakeCurrent failed"
            );

            let clear_color: Option<unsafe extern "C" fn(f32, f32, f32, f32)> =
                gl_proc("glClearColor");
            let clear: Option<unsafe extern "C" fn(u32)> = gl_proc("glClear");
            let clear_color = clear_color.expect("glClearColor must resolve");
            let clear = clear.expect("glClear must resolve");
            clear_color(1.0, 0.0, 0.0, 1.0);
            clear(0x4000); // GL_COLOR_BUFFER_BIT

            let exported =
                export_surface_dmabuf(surface).expect("dmabuf export must succeed");
            assert!(exported.fd >= 0, "export returned an invalid fd");
            let rec = &*(surface as *const SurfaceRec);
            assert_eq!(exported.width, 64, "export width must match the surface");
            assert_eq!(exported.height, 48, "export height must match the surface");
            assert_eq!(rec.w, 64);
            assert_eq!(rec.h, 48);
            assert_eq!(exported.stride % 4, 0, "stride is not pixel aligned");
            assert!(exported.fourcc != 0, "export returned no fourcc");

            // We own the fd from the export.
            libc::close(exported.fd);

            assert_eq!(eglDestroySurface(dpy, surface), EGL_TRUE);
            assert_eq!(eglDestroyContext(dpy, ctx), EGL_TRUE);
        }
    }
}

#[cfg(test)]
mod thread_tests {
    use super::*;

    /// Two unrelated contexts (a game's and a WebView's) take turns on one
    /// surface. Binding framebuffer 0 must land on an FBO that exists in the
    /// context doing the binding; one shared FBO name raised
    /// GL_INVALID_OPERATION in the second context on ES3.
    #[test]
    fn unrelated_contexts_get_their_own_surface_framebuffer() {
        let Ok(host) = host() else { eprintln!("skip: no host EGL"); return; };
        let (Some(get_error), Some(make_current)) = (host.gl_get_error, Some(host.make_current)) else { return };
        type CheckStatus = unsafe extern "C" fn(u32) -> u32;
        let pointer = unsafe { eglGetProcAddress(c"glCheckFramebufferStatus".as_ptr()) };
        if pointer.is_null() { return; }
        let check_status: CheckStatus = unsafe { std::mem::transmute(pointer) };
        unsafe {
            let dpy = eglGetDisplay(std::ptr::null_mut());
            let (mut major, mut minor) = (0, 0);
            if dpy.is_null() || eglInitialize(dpy, &mut major, &mut minor) != EGL_TRUE { eprintln!("skip: no display"); return; }
            eglBindAPI(0x30A0);
            let attribs: [c_int; 5] = [0x3033, 0x0001, 0x3040, 0x0040, 0x3038]; // pbuffer, ES3
            let (mut config, mut count): (EGLConfig, c_int) = (std::ptr::null_mut(), 0);
            if eglChooseConfig(dpy, attribs.as_ptr(), &mut config, 1, &mut count) != EGL_TRUE || count < 1 { eprintln!("skip: no ES3 config"); return; }
            let context_attribs = [0x3098, 3, 0x3038];
            let first = eglCreateContext(dpy, config, std::ptr::null_mut(), context_attribs.as_ptr());
            let second = eglCreateContext(dpy, config, std::ptr::null_mut(), context_attribs.as_ptr());
            let surface_attribs = [0x3057, 32, 0x3056, 32, 0x3038];
            let surface = eglCreatePbufferSurface(dpy, config, surface_attribs.as_ptr());
            assert!(!first.is_null() && !second.is_null() && !surface.is_null());
            for context in [first, second, first] {
                assert_eq!(eglMakeCurrent(dpy, surface, surface, context), EGL_TRUE);
                while get_error() != 0 {}
                glBindFramebuffer(GL_FRAMEBUFFER, 0);
                assert_eq!(get_error(), 0, "binding the default framebuffer failed in context {context:p}");
                // A name from another context binds without an error on some
                // drivers (NVIDIA), as an empty incomplete framebuffer.
                assert_eq!(check_status(GL_FRAMEBUFFER), 0x8CD5, "framebuffer incomplete in context {context:p}");
            }
            let _ = make_current;
            eglMakeCurrent(dpy, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            eglDestroySurface(dpy, surface);
            eglDestroyContext(dpy, first);
            eglDestroyContext(dpy, second);
        }
    }

    /// eglMakeCurrent re-targets framebuffer 0 only: an app FBO bound in a
    /// context stays bound across a switch to another context and back
    /// (Chromium's WebGL contexts rely on this), while a context that had
    /// the default framebuffer bound gets the surface FBO again.
    #[test]
    fn make_current_keeps_the_contexts_own_framebuffer_binding() {
        let Ok(host) = host() else { eprintln!("skip: no host EGL"); return; };
        let Some(gen_framebuffers) = host.gl_gen_framebuffers else { return };
        unsafe {
            let dpy = eglGetDisplay(std::ptr::null_mut());
            let (mut major, mut minor) = (0, 0);
            if dpy.is_null() || eglInitialize(dpy, &mut major, &mut minor) != EGL_TRUE { eprintln!("skip: no display"); return; }
            eglBindAPI(0x30A0);
            let attribs: [c_int; 5] = [0x3033, 0x0001, 0x3040, 0x0040, 0x3038];
            let (mut config, mut count): (EGLConfig, c_int) = (std::ptr::null_mut(), 0);
            if eglChooseConfig(dpy, attribs.as_ptr(), &mut config, 1, &mut count) != EGL_TRUE || count < 1 { eprintln!("skip: no ES3 config"); return; }
            let context_attribs = [0x3098, 3, 0x3038];
            let app = eglCreateContext(dpy, config, std::ptr::null_mut(), context_attribs.as_ptr());
            let other = eglCreateContext(dpy, config, app, context_attribs.as_ptr());
            let surface_attribs = [0x3057, 16, 0x3056, 16, 0x3038];
            let surface = eglCreatePbufferSurface(dpy, config, surface_attribs.as_ptr());
            assert!(!app.is_null() && !other.is_null() && !surface.is_null());

            assert_eq!(eglMakeCurrent(dpy, surface, surface, app), EGL_TRUE);
            let surface_fbo = host_get_integer(host, 0x8CA6) as u32;
            assert_ne!(surface_fbo, 0, "the surface FBO stands in for framebuffer 0");
            let mut own = 0u32;
            gen_framebuffers(1, &mut own);
            glBindFramebuffer(GL_FRAMEBUFFER, own);
            assert_eq!(eglMakeCurrent(dpy, surface, surface, other), EGL_TRUE);
            assert_eq!(eglMakeCurrent(dpy, surface, surface, app), EGL_TRUE);
            assert_eq!(host_get_integer(host, 0x8CA6) as u32, own, "draw binding survives the switch");
            assert_eq!(host_get_integer(host, 0x8CAA) as u32, own, "read binding survives the switch");

            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            assert_eq!(eglMakeCurrent(dpy, surface, surface, other), EGL_TRUE);
            assert_eq!(eglMakeCurrent(dpy, surface, surface, app), EGL_TRUE);
            assert_eq!(host_get_integer(host, 0x8CA6) as u32, surface_fbo, "framebuffer 0 follows the surface");

            eglMakeCurrent(dpy, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            eglDestroySurface(dpy, surface);
            eglDestroyContext(dpy, other);
            eglDestroyContext(dpy, app);
        }
    }

    /// The current surface is per thread: another thread making its own
    /// surface current must not change what this thread binds as FBO 0.
    #[test]
    fn current_surface_is_per_thread_and_forgets_destroyed_surfaces() {
        let mine = 0x1000usize;
        let theirs = 0x2000usize;
        live_surfaces().lock().unwrap().extend([mine, theirs]);
        CURRENT_SURFACE.with(|c| c.set(mine));
        std::thread::spawn(move || {
            assert_eq!(current_surface(), 0, "a new thread has no current surface");
            CURRENT_SURFACE.with(|c| c.set(theirs));
            assert_eq!(current_surface(), theirs);
        }).join().unwrap();
        assert_eq!(current_surface(), mine);
        live_surfaces().lock().unwrap().remove(&mine);
        assert_eq!(current_surface(), 0, "a destroyed surface is never handed out");
        live_surfaces().lock().unwrap().remove(&theirs);
    }
}
