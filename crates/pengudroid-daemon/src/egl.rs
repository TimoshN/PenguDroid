//! EGL presenter for guest Hybris/gralloc buffers.
//!
//! The guest renders into a native allocation and sends its complete
//! `native_handle_t` to the daemon. The daemon imports that allocation as an
//! `EGLImage`, draws a textured quad onto the window's real `EGLSurface`,
//! and presents it with `eglSwapBuffers`. No `wl_shm` copy or
//! manual `android_wlegl` buffer attachment is involved.

#![cfg(feature = "wayland")]

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use wayland_client::Proxy;

const EGL_TRUE: u32 = 1;
const EGL_NO_CONTEXT: *mut c_void = std::ptr::null_mut();
const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_CONTEXT_CLIENT_VERSION: c_int = 0x3098;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_PLATFORM_WAYLAND: u32 = 0x31D8;
const EGL_SURFACE_TYPE: c_int = 0x3033;
const EGL_WINDOW_BIT: c_int = 0x0004;
const EGL_RENDERABLE_TYPE: c_int = 0x3040;
const EGL_OPENGL_ES2_BIT: c_int = 0x0004;
const EGL_RED_SIZE: c_int = 0x3024;
const EGL_GREEN_SIZE: c_int = 0x3023;
const EGL_BLUE_SIZE: c_int = 0x3022;
const EGL_ALPHA_SIZE: c_int = 0x3021;
const EGL_NONE: c_int = 0x3038;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: c_int = 0x2601;
const GL_CLAMP_TO_EDGE: c_int = 0x812F;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_TRIANGLE_STRIP: u32 = 0x0005;
const GL_FLOAT: u32 = 0x1406;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_TEXTURE0: u32 = 0x84C0;

#[rustfmt::skip]
type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type EglGetDisplay = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EglGetPlatformDisplay = unsafe extern "C" fn(u32, *mut c_void, *const isize) -> *mut c_void;
type EglInitialize = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> u32;
type EglBindApi = unsafe extern "C" fn(u32) -> u32;
type EglChooseConfig =
    unsafe extern "C" fn(*mut c_void, *const c_int, *mut *mut c_void, c_int, *mut c_int) -> u32;
type EglCreateContext =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const c_int) -> *mut c_void;
type EglCreateWindowSurface =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const c_int) -> *mut c_void;
type EglDestroySurface = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type EglMakeCurrent =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> u32;
type EglSwapBuffers = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type EglGetError = unsafe extern "C" fn() -> c_int;
type EglCreateImageKhr =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const isize) -> *mut c_void;
type EglDestroyImageKhr = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;

type GlGenTextures = unsafe extern "C" fn(c_int, *mut u32);
type GlBindTexture = unsafe extern "C" fn(u32, u32);
type GlTexParameteri = unsafe extern "C" fn(u32, u32, c_int);
type GlEglImageTargetTexture2DOES = unsafe extern "C" fn(u32, *mut c_void);
type GlCreateShader = unsafe extern "C" fn(u32) -> u32;
type GlShaderSource = unsafe extern "C" fn(u32, c_int, *const *const c_char, *const c_int);
type GlCompileShader = unsafe extern "C" fn(u32);
type GlGetShaderiv = unsafe extern "C" fn(u32, u32, *mut c_int);
type GlGetShaderInfoLog = unsafe extern "C" fn(u32, c_int, *mut c_int, *mut c_char);
type GlCreateProgram = unsafe extern "C" fn() -> u32;
type GlAttachShader = unsafe extern "C" fn(u32, u32);
type GlLinkProgram = unsafe extern "C" fn(u32);
type GlGetProgramiv = unsafe extern "C" fn(u32, u32, *mut c_int);
type GlGetProgramInfoLog = unsafe extern "C" fn(u32, c_int, *mut c_int, *mut c_char);
type GlUseProgram = unsafe extern "C" fn(u32);
type GlGetAttribLocation = unsafe extern "C" fn(u32, *const c_char) -> c_int;
type GlGetUniformLocation = unsafe extern "C" fn(u32, *const c_char) -> c_int;
type GlUniform1i = unsafe extern "C" fn(c_int, c_int);
type GlEnableVertexAttribArray = unsafe extern "C" fn(u32);
type GlVertexAttribPointer = unsafe extern "C" fn(u32, c_int, u32, u8, c_int, *const c_void);
type GlDrawArrays = unsafe extern "C" fn(u32, c_int, c_int);
type GlActiveTexture = unsafe extern "C" fn(u32);
type GlViewport = unsafe extern "C" fn(c_int, c_int, c_int, c_int);
type GlClear = unsafe extern "C" fn(u32);
type GlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type GlDeleteTextures = unsafe extern "C" fn(c_int, *const u32);
type GlGetError = unsafe extern "C" fn() -> u32;

struct Api {
    // Libraries stay loaded for the presenter's whole lifetime. In particular,
    // EGLImage/texture destruction must never race a dlclose of their driver.
    _egl_library: *mut c_void,
    _gles_library: *mut c_void,
    egl_get_display: EglGetDisplay,
    egl_get_platform_display: Option<EglGetPlatformDisplay>,
    egl_initialize: EglInitialize,
    egl_bind_api: EglBindApi,
    egl_choose_config: EglChooseConfig,
    egl_create_context: EglCreateContext,
    egl_create_window_surface: EglCreateWindowSurface,
    egl_destroy_surface: EglDestroySurface,
    egl_make_current: EglMakeCurrent,
    egl_swap_buffers: EglSwapBuffers,
    egl_get_error: EglGetError,
    egl_create_image: EglCreateImageKhr,
    egl_destroy_image: EglDestroyImageKhr,
    gl_gen_textures: GlGenTextures,
    gl_bind_texture: GlBindTexture,
    gl_tex_parameteri: GlTexParameteri,
    gl_image_target_texture: GlEglImageTargetTexture2DOES,
    gl_create_shader: GlCreateShader,
    gl_shader_source: GlShaderSource,
    gl_compile_shader: GlCompileShader,
    gl_get_shaderiv: GlGetShaderiv,
    gl_get_shader_info_log: GlGetShaderInfoLog,
    gl_create_program: GlCreateProgram,
    gl_attach_shader: GlAttachShader,
    gl_link_program: GlLinkProgram,
    gl_get_programiv: GlGetProgramiv,
    gl_get_program_info_log: GlGetProgramInfoLog,
    gl_use_program: GlUseProgram,
    gl_get_attrib_location: GlGetAttribLocation,
    gl_get_uniform_location: GlGetUniformLocation,
    gl_uniform1i: GlUniform1i,
    gl_enable_vertex_attrib_array: GlEnableVertexAttribArray,
    gl_vertex_attrib_pointer: GlVertexAttribPointer,
    gl_draw_arrays: GlDrawArrays,
    gl_active_texture: GlActiveTexture,
    gl_viewport: GlViewport,
    gl_clear: GlClear,
    gl_clear_color: GlClearColor,
    gl_delete_textures: GlDeleteTextures,
    gl_get_error: GlGetError,
}

unsafe fn symbol(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let name = CString::new(name).ok()?;
    let pointer = libc::dlsym(handle, name.as_ptr());
    (!pointer.is_null()).then_some(pointer)
}

unsafe fn cast_function<T: Copy>(pointer: *mut c_void, name: &str) -> Result<T, String> {
    if pointer.is_null() {
        return Err(format!("{name} is unavailable"));
    }
    if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
        return Err(format!("{name}: unexpected function pointer size"));
    }
    Ok(std::mem::transmute_copy(&pointer))
}

unsafe fn resolve<T: Copy>(
    library: *mut c_void,
    get_proc: Option<EglGetProcAddress>,
    name: &str,
) -> Result<T, String> {
    let pointer = match symbol(library, name) {
        Some(pointer) => pointer,
        None => {
            let Some(get_proc) = get_proc else {
                return Err(format!("{name} is unavailable"));
            };
            let name = CString::new(name).unwrap();
            get_proc(name.as_ptr())
        }
    };
    cast_function(pointer, name)
}

impl Api {
    unsafe fn load() -> Result<Self, String> {
        let egl = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        if egl.is_null() {
            return Err("libEGL.so.1 unavailable".into());
        }
        let gles = libc::dlopen(
            c"libGLESv2.so.2".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if gles.is_null() {
            return Err("libGLESv2.so.2 unavailable".into());
        }
        let get_proc: EglGetProcAddress = cast_function(
            symbol(egl, "eglGetProcAddress").ok_or("eglGetProcAddress unavailable")?,
            "eglGetProcAddress",
        )?;
        let egl_get_platform_display = match symbol(egl, "eglGetPlatformDisplay") {
            Some(pointer) => Some(cast_function(pointer, "eglGetPlatformDisplay")?),
            None => None,
        };

        macro_rules! egl_fn {
            ($name:literal, $ty:ty) => {
                resolve(egl, Some(get_proc), $name)?
            };
        }
        macro_rules! gl_fn {
            ($name:literal, $ty:ty) => {
                resolve(gles, Some(get_proc), $name)?
            };
        }
        Ok(Self {
            _egl_library: egl,
            _gles_library: gles,
            egl_get_display: egl_fn!("eglGetDisplay", EglGetDisplay),
            egl_get_platform_display,
            egl_initialize: egl_fn!("eglInitialize", EglInitialize),
            egl_bind_api: egl_fn!("eglBindAPI", EglBindApi),
            egl_choose_config: egl_fn!("eglChooseConfig", EglChooseConfig),
            egl_create_context: egl_fn!("eglCreateContext", EglCreateContext),
            egl_create_window_surface: egl_fn!("eglCreateWindowSurface", EglCreateWindowSurface),
            egl_destroy_surface: egl_fn!("eglDestroySurface", EglDestroySurface),
            egl_make_current: egl_fn!("eglMakeCurrent", EglMakeCurrent),
            egl_swap_buffers: egl_fn!("eglSwapBuffers", EglSwapBuffers),
            egl_get_error: egl_fn!("eglGetError", EglGetError),
            egl_create_image: egl_fn!("eglCreateImageKHR", EglCreateImageKhr),
            egl_destroy_image: egl_fn!("eglDestroyImageKHR", EglDestroyImageKhr),
            gl_gen_textures: gl_fn!("glGenTextures", GlGenTextures),
            gl_bind_texture: gl_fn!("glBindTexture", GlBindTexture),
            gl_tex_parameteri: gl_fn!("glTexParameteri", GlTexParameteri),
            gl_image_target_texture: gl_fn!(
                "glEGLImageTargetTexture2DOES",
                GlEglImageTargetTexture2DOES
            ),
            gl_create_shader: gl_fn!("glCreateShader", GlCreateShader),
            gl_shader_source: gl_fn!("glShaderSource", GlShaderSource),
            gl_compile_shader: gl_fn!("glCompileShader", GlCompileShader),
            gl_get_shaderiv: gl_fn!("glGetShaderiv", GlGetShaderiv),
            gl_get_shader_info_log: gl_fn!("glGetShaderInfoLog", GlGetShaderInfoLog),
            gl_create_program: gl_fn!("glCreateProgram", GlCreateProgram),
            gl_attach_shader: gl_fn!("glAttachShader", GlAttachShader),
            gl_link_program: gl_fn!("glLinkProgram", GlLinkProgram),
            gl_get_programiv: gl_fn!("glGetProgramiv", GlGetProgramiv),
            gl_get_program_info_log: gl_fn!("glGetProgramInfoLog", GlGetProgramInfoLog),
            gl_use_program: gl_fn!("glUseProgram", GlUseProgram),
            gl_get_attrib_location: gl_fn!("glGetAttribLocation", GlGetAttribLocation),
            gl_get_uniform_location: gl_fn!("glGetUniformLocation", GlGetUniformLocation),
            gl_uniform1i: gl_fn!("glUniform1i", GlUniform1i),
            gl_enable_vertex_attrib_array: gl_fn!(
                "glEnableVertexAttribArray",
                GlEnableVertexAttribArray
            ),
            gl_vertex_attrib_pointer: gl_fn!("glVertexAttribPointer", GlVertexAttribPointer),
            gl_draw_arrays: gl_fn!("glDrawArrays", GlDrawArrays),
            gl_active_texture: gl_fn!("glActiveTexture", GlActiveTexture),
            gl_viewport: gl_fn!("glViewport", GlViewport),
            gl_clear: gl_fn!("glClear", GlClear),
            gl_clear_color: gl_fn!("glClearColor", GlClearColor),
            gl_delete_textures: gl_fn!("glDeleteTextures", GlDeleteTextures),
            gl_get_error: gl_fn!("glGetError", GlGetError),
        })
    }

    unsafe fn error(&self) -> c_int {
        (self.egl_get_error)()
    }
}

type GrallocImport = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type GrallocRetain = unsafe extern "C" fn(*mut c_void) -> c_int;
type GrallocRelease = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type GrallocInitialize = unsafe extern "C" fn(c_int);

/// How a guest native handle is registered with this process's gralloc module.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GrallocImportMode {
    /// Modern libhybris: `hybris_gralloc_import_buffer(handle, &out)` returns a
    /// process-local handle reference.
    Import,
    /// Gralloc0-era libhybris (ARM32 devices): only
    /// `hybris_gralloc_retain(handle)` (registerBuffer) exists. The rebuilt
    /// handle itself becomes the retained reference; release(handle, 0)
    /// unregisters it.
    Retain,
}

struct GrallocApi {
    /// Extra libraries kept loaded for the lifetime of the API (the
    /// eglplatformcommon preload that provides native_handle_* on older
    /// libhybris builds).
    _libraries: Vec<*mut c_void>,
    import_mode: GrallocImportMode,
    import_buffer: Option<GrallocImport>,
    retain: Option<GrallocRetain>,
    release: GrallocRelease,
}

impl GrallocApi {
    unsafe fn load() -> Result<Self, String> {
        let open_gralloc = || {
            libc::dlopen(c"libgralloc.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
        };
        let mut library = open_gralloc();
        let mut preload: Vec<*mut c_void> = Vec::new();
        if library.is_null() {
            // Older libhybris images (ARM32) link libgralloc.so.1 and
            // libhybris-eglplatformcommon with BIND_NOW and a circular
            // dependency: libgralloc needs native_handle_create/close/delete,
            // which on those images only eglplatformcommon exports, while
            // eglplatformcommon needs hybris_gralloc_retain from libgralloc.
            // They can only resolve as one connected graph. Loading the
            // Wayland EGL platform module first brings that graph in as a
            // set - the same way every working Hybris client on such images
            // already loads it - after which a direct libgralloc open finds
            // the graph member and resolves.
            let platform = if cfg!(target_arch = "arm") {
                "/usr/lib/libhybris/eglplatform_wayland.so"
            } else {
                "/usr/lib64/libhybris/eglplatform_wayland.so"
            };
            for name in [platform, "libhybris-eglplatformcommon.so.1"] {
                let Ok(cname) = CString::new(name) else { continue };
                let loaded = libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
                if loaded.is_null() {
                    continue;
                }
                preload.push(loaded);
                library = open_gralloc();
                if !library.is_null() {
                    break;
                }
            }
        }
        if library.is_null() {
            return Err("libgralloc.so.1 unavailable".into());
        }
        let initialize: GrallocInitialize = cast_function(
            symbol(library, "hybris_gralloc_initialize")
                .ok_or("hybris_gralloc_initialize unavailable")?,
            "hybris_gralloc_initialize",
        )?;
        // libhybris declares hybris_gralloc_initialize(int) as void.
        initialize(0);
        let import_buffer = symbol(library, "hybris_gralloc_import_buffer")
            .map(|ptr| cast_function::<GrallocImport>(ptr, "hybris_gralloc_import_buffer"))
            .transpose()?;
        let retain = symbol(library, "hybris_gralloc_retain")
            .map(|ptr| cast_function::<GrallocRetain>(ptr, "hybris_gralloc_retain"))
            .transpose()?;
        let release: GrallocRelease = cast_function(
            symbol(library, "hybris_gralloc_release")
                .ok_or("hybris_gralloc_release unavailable")?,
            "hybris_gralloc_release",
        )?;
        let import_mode = if import_buffer.is_some() {
            GrallocImportMode::Import
        } else {
            GrallocImportMode::Retain
        };
        crate::info!(
            "gralloc import mode: {}",
            match import_mode {
                GrallocImportMode::Import => "hybris_gralloc_import_buffer",
                GrallocImportMode::Retain => "hybris_gralloc_retain",
            }
        );
        Ok(Self {
            _libraries: preload,
            import_mode,
            import_buffer,
            retain,
            release,
        })
    }

    /// Register the guest handle with this process's gralloc module and return
    /// the reference to keep until `release` is called.
    unsafe fn import(&self, handle: *mut c_void, out: *mut *mut c_void) -> Result<(), String> {
        match self.import_mode {
            GrallocImportMode::Import => {
                let import = self.import_buffer.ok_or("import unavailable")?;
                let rc = import(handle, out);
                if rc != 0 || (*out).is_null() {
                    return Err(format!(
                        "hybris_gralloc_import_buffer failed: rc={rc} handle={handle:p}"
                    ));
                }
            }
            GrallocImportMode::Retain => {
                let retain = self.retain.ok_or("retain unavailable")?;
                let rc = retain(handle);
                if rc != 0 {
                    return Err(format!(
                        "hybris_gralloc_retain failed: rc={rc} handle={handle:p}"
                    ));
                }
                *out = handle;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Program {
    id: u32,
    position: u32,
    uv: u32,
    texture: c_int,
}

struct State {
    display: *mut c_void,
    context: *mut c_void,
    config: *mut c_void,
    program: Option<Program>,
}

struct ImportedTexture {
    id: u32,
    // Imported (not allocated here): release with was_allocated=0 after the GL
    // texture is gone. Keeping it alive also keeps the native allocation valid.
    gralloc_handle: *mut c_void,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct BufferKey {
    ints: Vec<i32>,
    fd_identity: Option<(u64, u64)>,
    width: u32,
    height: u32,
    stride: u32,
    format: i32,
    usage: i32,
}

struct SurfaceEgl {
    window: WlEglWindow,
    surface: *mut c_void,
    width: u32,
    height: u32,
    textures: HashMap<BufferKey, ImportedTexture>,
}

/// Minimal `wl_egl_window` owner. The `wayland-egl` crate lazily dlopens
/// `libwayland-egl.so.1` through `libloading` on first use; on the ARM32
/// Aurora image that first-use path faulted inside the daemon with a corrupted
/// stack before any EGL call, so the stable public ABI (unchanged since 2012)
/// is driven directly with `dlsym` on the already-loaded library instead.
#[repr(C)]
struct WlEglWindowLayout {
    surface: *mut c_void,
    width: i32,
    height: i32,
    dx: i32,
    dy: i32,
    attached_width: i32,
    attached_height: i32,
    driver_private: *mut c_void,
}

type WlEglWindowCreate = unsafe extern "C" fn(*mut c_void, i32, i32) -> *mut WlEglWindowLayout;
type WlEglWindowResize = unsafe extern "C" fn(*mut WlEglWindowLayout, i32, i32, i32, i32);
type WlEglWindowDestroy = unsafe extern "C" fn(*mut WlEglWindowLayout);

#[derive(Clone, Copy)]
struct WlEglApi {
    create: WlEglWindowCreate,
    resize: WlEglWindowResize,
    destroy: WlEglWindowDestroy,
}

impl WlEglApi {
    unsafe fn load() -> Result<Self, String> {
        let library = libc::dlopen(
            c"libwayland-egl.so.1".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if library.is_null() {
            return Err("libwayland-egl.so.1 unavailable".into());
        }
        let create = symbol(library, "wl_egl_window_create")
            .ok_or("wl_egl_window_create unavailable")?;
        let resize = symbol(library, "wl_egl_window_resize")
            .ok_or("wl_egl_window_resize unavailable")?;
        let destroy = symbol(library, "wl_egl_window_destroy")
            .ok_or("wl_egl_window_destroy unavailable")?;
        Ok(Self {
            create: cast_function(create, "wl_egl_window_create")?,
            resize: cast_function(resize, "wl_egl_window_resize")?,
            destroy: cast_function(destroy, "wl_egl_window_destroy")?,
        })
    }
}

struct WlEglWindow {
    api: WlEglApi,
    ptr: *mut WlEglWindowLayout,
}

impl WlEglWindow {
    unsafe fn new(
        wl_surface: &wayland_client::protocol::wl_surface::WlSurface,
        width: i32,
        height: i32,
    ) -> Result<Self, String> {
        let api = WlEglApi::load()?;
        let ptr = (api.create)(wl_surface.id().as_ptr().cast(), width, height);
        if ptr.is_null() {
            return Err("wl_egl_window_create returned null".into());
        }
        Ok(Self { api, ptr })
    }

    fn ptr(&self) -> *mut WlEglWindowLayout {
        self.ptr
    }

    fn resize(&self, width: i32, height: i32, dx: i32, dy: i32) {
        unsafe { (self.api.resize)(self.ptr, width, height, dx, dy) }
    }
}

impl Drop for WlEglWindow {
    fn drop(&mut self) {
        unsafe { (self.api.destroy)(self.ptr) }
    }
}

pub struct Presenter {
    api: Api,
    gralloc: GrallocApi,
    state: State,
    surfaces: HashMap<u32, SurfaceEgl>,
    red_test: bool,
}

// The daemon calls the presenter only from its graphics thread. Raw driver
// pointers are never handed to guest or connection threads.
unsafe impl Send for Presenter {}

const VERT: &str = "attribute vec2 aPos; attribute vec2 aUv; varying vec2 vUv;\
    void main(){ vUv = aUv; gl_Position = vec4(aPos, 0.0, 1.0); }";
const FRAG: &str = "precision mediump float; varying vec2 vUv; uniform sampler2D uTex;\
    void main(){ gl_FragColor = texture2D(uTex, vUv); }";

unsafe fn shader_log(api: &Api, shader: u32) -> String {
    let mut bytes = vec![0u8; 4096];
    let mut length = 0;
    (api.gl_get_shader_info_log)(
        shader,
        bytes.len() as c_int,
        &mut length,
        bytes.as_mut_ptr().cast(),
    );
    String::from_utf8_lossy(&bytes[..length.max(0) as usize]).into_owned()
}

unsafe fn compile_shader(api: &Api, kind: u32, source: &str) -> Result<u32, String> {
    let shader = (api.gl_create_shader)(kind);
    if shader == 0 {
        return Err("glCreateShader returned 0".into());
    }
    let source = CString::new(source).unwrap();
    let pointer = source.as_ptr();
    (api.gl_shader_source)(shader, 1, &pointer, std::ptr::null());
    (api.gl_compile_shader)(shader);
    let mut status = 0;
    (api.gl_get_shaderiv)(shader, GL_COMPILE_STATUS, &mut status);
    if status == 0 {
        return Err(format!(
            "shader compile failed: {}",
            shader_log(api, shader)
        ));
    }
    Ok(shader)
}

unsafe fn create_program(api: &Api) -> Result<Program, String> {
    let vertex = compile_shader(api, GL_VERTEX_SHADER, VERT)?;
    let fragment = compile_shader(api, GL_FRAGMENT_SHADER, FRAG)?;
    let program = (api.gl_create_program)();
    (api.gl_attach_shader)(program, vertex);
    (api.gl_attach_shader)(program, fragment);
    (api.gl_link_program)(program);
    let mut linked = 0;
    (api.gl_get_programiv)(program, GL_LINK_STATUS, &mut linked);
    if linked == 0 {
        let mut bytes = vec![0u8; 4096];
        let mut length = 0;
        (api.gl_get_program_info_log)(
            program,
            bytes.len() as c_int,
            &mut length,
            bytes.as_mut_ptr().cast(),
        );
        return Err(format!(
            "program link failed: {}",
            String::from_utf8_lossy(&bytes[..length.max(0) as usize])
        ));
    }
    let position = (api.gl_get_attrib_location)(program, c"aPos".as_ptr());
    let uv = (api.gl_get_attrib_location)(program, c"aUv".as_ptr());
    let texture = (api.gl_get_uniform_location)(program, c"uTex".as_ptr());
    if position < 0 || uv < 0 || texture < 0 {
        return Err(format!(
            "shader interface missing: aPos={position} aUv={uv} uTex={texture}"
        ));
    }
    Ok(Program {
        id: program,
        position: position as u32,
        uv: uv as u32,
        texture,
    })
}

fn native_handle_words(fds: &[RawFd], ints: &[i32]) -> Vec<i32> {
    let mut words = Vec::with_capacity(3 + fds.len() + ints.len());
    words.extend_from_slice(&[12, fds.len() as i32, ints.len() as i32]);
    words.extend_from_slice(fds);
    words.extend_from_slice(ints);
    words
}

fn buffer_key(
    fds: &[OwnedFd],
    ints: &[i32],
    width: u32,
    height: u32,
    stride: u32,
    format: i32,
    usage: i32,
) -> BufferKey {
    let fd_identity = fds.first().and_then(|fd| {
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        (unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } == 0)
            .then_some((stat.st_dev as u64, stat.st_ino as u64))
    });
    BufferKey {
        ints: ints.to_vec(),
        fd_identity,
        width,
        height,
        stride,
        format,
        usage,
    }
}

unsafe fn import_texture(
    api: &Api,
    display: *mut c_void,
    handle: *mut c_void,
    width: u32,
    height: u32,
    stride: u32,
    format: i32,
    usage: i32,
) -> Result<u32, String> {
    let mut native = NativeWindowBuffer::new(handle, width, height, stride, format, usage);
    let trace = std::env::var_os("PENGUDROID_TRACE_EGL").is_some();
    if trace {
        eprintln!("[pengudroid-daemon:egl]   import_texture: handle={handle:p} {width}x{height} stride={stride} format={format:#x} usage={usage:#x}");
    }
    let image = (api.egl_create_image)(
        display,
        EGL_NO_CONTEXT,
        EGL_NATIVE_BUFFER_ANDROID,
        &mut native as *mut NativeWindowBuffer as *mut c_void,
        std::ptr::null(),
    );
    if image.is_null() {
        return Err(format!("eglCreateImageKHR failed: {:#x}", api.error()));
    }
    if trace {
        eprintln!("[pengudroid-daemon:egl]   eglCreateImageKHR -> {image:p}");
    }

    let mut texture = 0;
    (api.gl_gen_textures)(1, &mut texture);
    if texture == 0 {
        (api.egl_destroy_image)(display, image);
        return Err("glGenTextures returned 0".into());
    }
    (api.gl_bind_texture)(GL_TEXTURE_2D, texture);
    (api.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    (api.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    (api.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    (api.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    if trace {
        eprintln!("[pengudroid-daemon:egl]   glEGLImageTargetTexture2DOES(texture={texture})");
    }
    (api.gl_image_target_texture)(GL_TEXTURE_2D, image);
    let error = (api.gl_get_error)();
    (api.egl_destroy_image)(display, image);
    if error != 0 {
        (api.gl_delete_textures)(1, &texture);
        return Err(format!(
            "glEGLImageTargetTexture2DOES failed: GL error {error:#x}"
        ));
    }
    Ok(texture)
}

impl Presenter {
    pub fn new(wl_display: *mut c_void) -> Result<Self, String> {
        let api = unsafe { Api::load()? };
        unsafe {
            // Reuse the daemon's wl_display: EGL and wayland-client must operate
            // on the same native connection when attaching the swapped buffer.
            let display = match api.egl_get_platform_display {
                Some(get_platform) => {
                    get_platform(EGL_PLATFORM_WAYLAND, wl_display, std::ptr::null())
                }
                None => (api.egl_get_display)(wl_display),
            };
            if display.is_null() {
                return Err(format!("eglGetDisplay returned null: {:#x}", api.error()));
            }
            let (mut major, mut minor) = (0, 0);
            if (api.egl_initialize)(display, &mut major, &mut minor) != EGL_TRUE {
                return Err(format!("eglInitialize failed: {:#x}", api.error()));
            }
            if (api.egl_bind_api)(EGL_OPENGL_ES_API) != EGL_TRUE {
                return Err(format!("eglBindAPI failed: {:#x}", api.error()));
            }
            let attributes = [
                EGL_SURFACE_TYPE,
                EGL_WINDOW_BIT,
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_ES2_BIT,
                EGL_RED_SIZE,
                8,
                EGL_GREEN_SIZE,
                8,
                EGL_BLUE_SIZE,
                8,
                EGL_ALPHA_SIZE,
                8,
                EGL_NONE,
            ];
            let mut config = std::ptr::null_mut();
            let mut count = 0;
            if (api.egl_choose_config)(display, attributes.as_ptr(), &mut config, 1, &mut count)
                != EGL_TRUE
                || count < 1
            {
                return Err(format!("eglChooseConfig failed: {:#x}", api.error()));
            }
            let gralloc = GrallocApi::load()?;
            let red_test = std::env::var("PENGUDROID_EGL_RED_TEST")
                .ok()
                .is_some_and(|value| !value.is_empty() && value != "0");
            crate::info!(
                "EGL presenter initialized: EGL {major}.{minor}, mode={}",
                if red_test {
                    "red diagnostic"
                } else {
                    "guest UI"
                }
            );
            Ok(Self {
                api,
                gralloc,
                state: State {
                    display,
                    context: EGL_NO_CONTEXT,
                    config,
                    program: None,
                },
                surfaces: HashMap::new(),
                red_test,
            })
        }
    }

    pub fn create_surface(
        &mut self,
        id: u32,
        wl_surface: &wayland_client::protocol::wl_surface::WlSurface,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if self.surfaces.contains_key(&id) {
            return Ok(());
        }
        if width == 0 || height == 0 {
            return Err(format!("invalid EGL surface size {width}x{height}"));
        }
        // Raw marker lines (no formatting machinery) so the failing step can be
        // identified even when the crash lies inside a format path.
        macro_rules! mark {
            ($lit:literal) => {{
                unsafe { libc::write(2, concat!("[pengudroid-daemon:egl] create_surface: ", $lit, "\n").as_ptr().cast(), concat!("[pengudroid-daemon:egl] create_surface: ", $lit, "\n").len()) };
            }};
        }
        if std::env::var_os("PENGUDROID_TRACE_EGL").is_some() {
            mark!("enter");
        }
        // The EGL window must be created before any EGL call on this surface.
        let window = unsafe { WlEglWindow::new(wl_surface, width as i32, height as i32) }
            .map_err(|error| format!("wl_egl_window_create: {error}"))?;
        if std::env::var_os("PENGUDROID_TRACE_EGL").is_some() {
            mark!("wl_egl_window_create ok");
        }
        unsafe {
            let api = &self.api;
            let surface = (api.egl_create_window_surface)(
                self.state.display,
                self.state.config,
                window.ptr() as *mut c_void,
                std::ptr::null(),
            );
            if surface.is_null() {
                return Err(format!("eglCreateWindowSurface failed: {:#x}", api.error()));
            }
            if std::env::var_os("PENGUDROID_TRACE_EGL").is_some() {
                mark!("eglCreateWindowSurface ok");
            }

            // Keep the working libhybris order: window surface first, then the
            // ES2 context and shader program.
            let initialized = (|| {
                if self.state.context == EGL_NO_CONTEXT {
                    let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
                    let context = (api.egl_create_context)(
                        self.state.display,
                        self.state.config,
                        EGL_NO_CONTEXT,
                        context_attributes.as_ptr(),
                    );
                    if context == EGL_NO_CONTEXT {
                        return Err(format!("eglCreateContext failed: {:#x}", api.error()));
                    }
                    self.state.context = context;
                }
                if std::env::var_os("PENGUDROID_TRACE_EGL").is_some() {
                    mark!("eglCreateContext ok");
                }
                if (api.egl_make_current)(self.state.display, surface, surface, self.state.context)
                    != EGL_TRUE
                {
                    return Err(format!("eglMakeCurrent failed: {:#x}", api.error()));
                }
                if std::env::var_os("PENGUDROID_TRACE_EGL").is_some() {
                    mark!("eglMakeCurrent ok");
                }
                if self.state.program.is_none() {
                    if std::env::var_os("PENGUDROID_TRACE_EGL").is_some() {
                        mark!("create_program enter");
                    }
                    self.state.program = Some(create_program(api)?);
                    if std::env::var_os("PENGUDROID_TRACE_EGL").is_some() {
                        mark!("create_program ok");
                    }
                }
                Ok(())
            })();
            if let Err(error) = initialized {
                (api.egl_destroy_surface)(self.state.display, surface);
                return Err(error);
            }
            self.surfaces.insert(
                id,
                SurfaceEgl {
                    window,
                    surface,
                    width,
                    height,
                    textures: HashMap::new(),
                },
            );
            crate::info!("EGL presenter: surface {id} ready ({width}x{height})");
        }
        Ok(())
    }

    /// Import one guest allocation on first use, then draw its current pixels
    /// and swap. Subsequent frames reuse the texture and imported gralloc ref.
    pub fn present(
        &mut self,
        id: u32,
        fds: &[OwnedFd],
        ints: &[i32],
        width: u32,
        height: u32,
        stride: u32,
        format: i32,
        usage: i32,
    ) -> Result<(), String> {
        if width == 0 || height == 0 || stride < width {
            return Err(format!(
                "invalid native buffer layout: {width}x{height}, stride={stride}"
            ));
        }
        if fds.is_empty() || fds.len() > 64 || ints.len() > 1024 {
            return Err(format!(
                "invalid native handle: fds={} ints={}",
                fds.len(),
                ints.len()
            ));
        }
        if fds.iter().any(|fd| fd.as_raw_fd() < 0) {
            return Err("native handle contains an invalid fd".into());
        }

        let Self {
            api,
            gralloc,
            state,
            surfaces,
            red_test,
        } = self;
        let surface = surfaces.get_mut(&id).ok_or("EGL surface missing")?;
        let trace = std::env::var_os("PENGUDROID_TRACE_EGL").is_some();
        let step = |name: &str| {
            if trace {
                eprintln!("[pengudroid-daemon:egl] present: {name}");
            }
        };
        unsafe {
            if surface.width != width || surface.height != height {
                surface.window.resize(width as i32, height as i32, 0, 0);
                surface.width = width;
                surface.height = height;
            }
            step("eglMakeCurrent");
            if (api.egl_make_current)(
                state.display,
                surface.surface,
                surface.surface,
                state.context,
            ) != EGL_TRUE
            {
                return Err(format!("eglMakeCurrent failed: {:#x}", api.error()));
            }

            if *red_test {
                (api.gl_clear_color)(1.0, 0.0, 0.0, 1.0);
                (api.gl_clear)(GL_COLOR_BUFFER_BIT);
                return swap(api, state.display, surface.surface);
            }

            let key = buffer_key(fds, ints, width, height, stride, format, usage);
            if !surface.textures.contains_key(&key) {
                let raw_fds: Vec<RawFd> = fds.iter().map(|fd| fd.as_raw_fd()).collect();
                let mut words = native_handle_words(&raw_fds, ints);
                let mut gralloc_handle = std::ptr::null_mut();
                step(&format!("gralloc import (fds={:?} ints={:?})", raw_fds, ints));
                if let Err(error) = gralloc.import(words.as_mut_ptr().cast(), &mut gralloc_handle) {
                    return Err(error);
                }
                step("eglCreateImageKHR(EGL_NATIVE_BUFFER_ANDROID)");
                let texture = import_texture(
                    api,
                    state.display,
                    gralloc_handle,
                    width,
                    height,
                    stride,
                    format,
                    usage,
                );
                step("import_texture returned");
                match texture {
                    Ok(texture) => {
                        surface.textures.insert(
                            key.clone(),
                            ImportedTexture {
                                id: texture,
                                gralloc_handle,
                            },
                        );
                        crate::info!(
                            "EGL presenter: imported guest buffer on surface {id}, textures={}",
                            surface.textures.len()
                        );
                    }
                    Err(error) => {
                        (gralloc.release)(gralloc_handle, 0);
                        return Err(error);
                    }
                }
            }
            let texture = surface.textures.get(&key).unwrap().id;
            let program = state.program.ok_or("GL program missing")?;
            step("draw");
            (api.gl_viewport)(0, 0, width as c_int, height as c_int);
            (api.gl_clear_color)(0.0, 0.0, 0.0, 1.0);
            (api.gl_clear)(GL_COLOR_BUFFER_BIT);
            (api.gl_use_program)(program.id);
            (api.gl_active_texture)(GL_TEXTURE0);
            (api.gl_bind_texture)(GL_TEXTURE_2D, texture);
            (api.gl_uniform1i)(program.texture, 0);

            // The device's gralloc/EGLImage path already presents the guest's
            // HWUI output top-down. Sample it directly: an extra V-flip here
            // produces an upside-down interface on Mali/Hybris.
            const VERTICES: [f32; 16] = [
                -1.0, -1.0, 0.0, 0.0, //
                1.0, -1.0, 1.0, 0.0, //
                -1.0, 1.0, 0.0, 1.0, //
                1.0, 1.0, 1.0, 1.0,
            ];
            (api.gl_enable_vertex_attrib_array)(program.position);
            (api.gl_enable_vertex_attrib_array)(program.uv);
            (api.gl_vertex_attrib_pointer)(
                program.position,
                2,
                GL_FLOAT,
                0,
                16,
                VERTICES.as_ptr().cast(),
            );
            (api.gl_vertex_attrib_pointer)(
                program.uv,
                2,
                GL_FLOAT,
                0,
                16,
                VERTICES.as_ptr().add(2).cast(),
            );
            (api.gl_draw_arrays)(GL_TRIANGLE_STRIP, 0, 4);
            let error = (api.gl_get_error)();
            if error != 0 {
                return Err(format!("guest textured draw failed: GL error {error:#x}"));
            }
            step("eglSwapBuffers");
            swap(api, state.display, surface.surface)
        }
    }

    pub fn destroy_surface(&mut self, id: u32) {
        let Some(mut surface) = self.surfaces.remove(&id) else {
            return;
        };
        unsafe {
            // Deletion needs a current GL context. Do it before EGLSurface and
            // wl_egl_window disappear, and before graphics destroys wl_surface.
            (self.api.egl_make_current)(
                self.state.display,
                surface.surface,
                surface.surface,
                self.state.context,
            );
            for (_, texture) in surface.textures.drain() {
                (self.api.gl_delete_textures)(1, &texture.id);
                (self.gralloc.release)(texture.gralloc_handle, 0);
            }
            (self.api.egl_destroy_surface)(self.state.display, surface.surface);
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.surfaces.keys().copied().collect();
        for id in ids {
            self.destroy_surface(id);
        }
        // Driver libraries and the shared EGLDisplay remain process-lifetime
        // objects. Other daemon graphics paths may still be using them.
    }
}

unsafe fn swap(api: &Api, display: *mut c_void, surface: *mut c_void) -> Result<(), String> {
    if (api.egl_swap_buffers)(display, surface) != EGL_TRUE {
        return Err(format!("eglSwapBuffers failed: {:#x}", api.error()));
    }
    Ok(())
}

/// Mirror of `ANativeWindowBuffer` for `EGL_NATIVE_BUFFER_ANDROID` import.
#[repr(C)]
struct NativeWindowBuffer {
    magic: i32,
    version: i32,
    reserved: [*mut c_void; 4],
    inc_ref: *mut c_void,
    dec_ref: *mut c_void,
    width: c_int,
    height: c_int,
    stride: c_int,
    format: c_int,
    usage_deprecated: c_int,
    layer_count: usize,
    reserved_ptr: *mut c_void,
    handle: *const c_void,
    usage: u64,
    reserved_proc: [*mut c_void; 7],
}

extern "C" fn noop_ref(_: *mut c_void) {}

impl NativeWindowBuffer {
    fn new(
        handle: *const c_void,
        width: u32,
        height: u32,
        stride: u32,
        format: i32,
        usage: i32,
    ) -> Self {
        Self {
            magic: i32::from_be_bytes(*b"_bfr"),
            version: std::mem::size_of::<NativeWindowBuffer>() as i32,
            reserved: [std::ptr::null_mut(); 4],
            inc_ref: noop_ref as *mut c_void,
            dec_ref: noop_ref as *mut c_void,
            width: width as c_int,
            height: height as c_int,
            stride: stride as c_int,
            format,
            usage_deprecated: usage,
            layer_count: 1,
            reserved_ptr: std::ptr::null_mut(),
            handle,
            usage: usage as u32 as u64,
            reserved_proc: [std::ptr::null_mut(); 7],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{native_handle_words, NativeWindowBuffer};

    #[test]
    fn native_handle_has_android_layout_and_payload() {
        let words = native_handle_words(&[10, 11], &[0x1234, -7]);
        assert_eq!(words, vec![12, 2, 2, 10, 11, 0x1234, -7]);
    }

    #[test]
    fn native_window_buffer_preserves_wire_metadata() {
        let handle = 1usize as *const std::ffi::c_void;
        let native = NativeWindowBuffer::new(handle, 720, 1280, 736, 5, 0x303);
        assert_eq!(native.magic, i32::from_be_bytes(*b"_bfr"));
        assert_eq!(
            native.version,
            std::mem::size_of::<NativeWindowBuffer>() as i32
        );
        assert_eq!(
            (native.width, native.height, native.stride),
            (720, 1280, 736)
        );
        assert_eq!(native.handle, handle);
        assert_eq!(native.format, 5);
        assert_eq!(native.usage, 0x303);
    }
}
