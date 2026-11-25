//! `android_wlegl` transport probe for Aurora / Sailfish compositors.
//!
//! Lipstick on Aurora publishes neither `zwp_linux_dmabuf_v1` nor `wl_drm`, so
//! every frame currently reaches the compositor through `wl_shm`: a gralloc
//! allocation is exposed as a mmap-able fd and the compositor reads it from CPU
//! memory (1080x2460x4 = 10.6 MB per frame). The device *does* advertise
//! `android_wlegl` v2, the Sailfish/hybris extension that hands an Android
//! native buffer to the compositor as a regular `wl_buffer`, which would let it
//! sample the buffer as a GPU texture instead.
//!
//! This command answers one question without touching the daemon or the guest
//! pipeline: does this compositor accept our gralloc allocation through
//! `android_wlegl`? It allocates a native buffer exactly like the gleshim
//! gralloc path (BGRA8888, HW_RENDER|HW_TEXTURE|SW_READ_OFTEN), fills it from
//! the CPU for now, presents it as a `wl_shell` toplevel and reports the
//! `wl_buffer.release` stream.
//!
//! Rendering into the same allocation with EGL (`EGL_NATIVE_BUFFER_ANDROID` ->
//! FBO) is the next step once the transport itself is verified; a CPU fill is
//! enough to prove or disprove it and keeps the probe independent of EGL.

pub mod protocol {
    pub mod imp {
        use wayland_client;
        use wayland_client::protocol::wl_buffer;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/wayland-android.xml");
        }

        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/wayland-android.xml");
    }
}

use std::ffi::c_void;
use std::os::fd::{BorrowedFd, RawFd};
use std::time::{Duration, Instant};

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shell, wl_shell_surface, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use self::protocol::imp::android_wlegl::AndroidWlegl;
use self::protocol::imp::android_wlegl_handle::AndroidWleglHandle;

/// `HAL_PIXEL_FORMAT_BGRA_8888`; the layout the compositor expects for a
/// texture-usable native buffer, and what the gleshim gralloc path allocates.
const FORMAT_BGRA_8888: i32 = 5;
/// `GRALLOC_USAGE_SW_READ_OFTEN | SW_WRITE_OFTEN | HW_TEXTURE | HW_RENDER`.
const USAGE_LINEAR_TEXTURE: u32 = 0x303;

type GrallocInitialize = unsafe extern "C" fn(i32);
type GrallocAllocate =
    unsafe extern "C" fn(u32, u32, i32, u32, *mut *mut c_void, *mut u32) -> i32;
type GrallocRelease = unsafe extern "C" fn(*mut c_void, i32);
type GrallocDeinitialize = unsafe extern "C" fn();
type GrallocLock =
    unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32, i32, *mut *mut c_void) -> i32;
type GrallocUnlock = unsafe extern "C" fn(*mut c_void) -> i32;

struct GrallocApi {
    initialize: GrallocInitialize,
    allocate: GrallocAllocate,
    release: GrallocRelease,
    deinitialize: GrallocDeinitialize,
    /// CPU writes into a gralloc allocation are invisible to the GPU until the
    /// driver flushes caches, which is what lock/unlock does. Without it the
    /// compositor keeps sampling the first frame it imported.
    lock: GrallocLock,
    unlock: GrallocUnlock,
}

/// One native buffer plus the pieces `android_wlegl` needs to rebuild the
/// Android `native_handle_t` on the compositor side.
struct NativeBuffer {
    api: GrallocApi,
    handle: *mut c_void,
    fds: Vec<RawFd>,
    ints: Vec<u8>,
    stride: u32,
    width: u32,
    height: u32,
    mapping: *mut u8,
    mapping_len: usize,
    /// The fd that mapped successfully; the wl_shm comparison presents this
    /// same allocation the way the current guest path does.
    shm_fd: RawFd,
}

impl NativeBuffer {
    unsafe fn allocate(width: u32, height: u32) -> Result<NativeBuffer, String> {
        let api = gralloc_api()?;
        let mut handle: *mut c_void = std::ptr::null_mut();
        let mut stride = 0u32;
        (api.initialize)(0);
        let rc = (api.allocate)(
            width,
            height,
            FORMAT_BGRA_8888,
            USAGE_LINEAR_TEXTURE,
            &mut handle,
            &mut stride,
        );
        if rc != 0 || handle.is_null() {
            (api.deinitialize)();
            return Err(format!(
                "hybris_gralloc_allocate({width}x{height}) failed: rc={rc} handle={handle:p}"
            ));
        }
        if stride == 0 {
            (api.release)(handle, 1);
            (api.deinitialize)();
            return Err("gralloc returned stride 0".into());
        }

        // native_handle_t layout: [version, numFds, numInts, data...]; data
        // holds the fds first, then the ints. android_wlegl sends the two parts
        // separately because Wayland cannot carry a variable fd array.
        let header = handle as *const i32;
        let num_fds = *header.add(1);
        let num_ints = *header.add(2);
        if num_fds < 1 || num_ints < 0 || num_fds > 64 || num_ints > 1024 {
            (api.release)(handle, 1);
            (api.deinitialize)();
            return Err(format!(
                "unexpected gralloc native handle: numFds={num_fds} numInts={num_ints}"
            ));
        }
        let mut fds = Vec::with_capacity(num_fds as usize);
        for index in 0..num_fds as usize {
            fds.push(*header.add(3 + index));
        }
        let ints_start = 3 + num_fds as usize;
        let mut ints = vec![0u8; num_ints as usize * 4];
        for index in 0..num_ints as usize {
            let word = *header.add(ints_start + index);
            ints[index * 4..index * 4 + 4].copy_from_slice(&word.to_ne_bytes());
        }

        // mmap-able fd for the CPU fill below; the same selection the gleshim
        // wl_shm path performs before handing the fd to the daemon.
        let length = stride as usize * height as usize * 4;
        let mut mapping: *mut u8 = std::ptr::null_mut();
        let mut mapped_len = 0usize;
        let mut shm_fd = -1;
        for &fd in &fds {
            if fd < 0 {
                continue;
            }
            let candidate = libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if candidate != libc::MAP_FAILED {
                mapping = candidate.cast();
                mapped_len = length;
                shm_fd = fd;
                break;
            }
        }
        if mapping.is_null() {
            (api.release)(handle, 1);
            (api.deinitialize)();
            return Err("no gralloc fd is mmap-able".into());
        }

        Ok(NativeBuffer {
            api,
            handle,
            fds,
            ints,
            stride,
            width,
            height,
            mapping,
            mapping_len: mapped_len,
            shm_fd,
        })
    }

    /// Fill the allocation with a moving pattern so a presented frame is
    /// visibly new rather than a repeated static image.
    ///
    /// The write goes through `hybris_gralloc_lock`/`unlock`: the compositor
    /// imports this buffer as a GPU texture once, and CPU stores without the
    /// driver's cache maintenance never reach it.
    unsafe fn paint(&mut self, frame: u32) {
        let mut vaddr: *mut c_void = std::ptr::null_mut();
        let rc = (self.api.lock)(
            self.handle,
            USAGE_LINEAR_TEXTURE as i32,
            0,
            0,
            self.width as i32,
            self.height as i32,
            &mut vaddr,
        );
        static LOCK_REPORTED: std::sync::Once = std::sync::Once::new();
        LOCK_REPORTED.call_once(|| {
            println!(
                "gralloc lock: rc={rc} vaddr={:p} (0 means CPU writes are cache-synchronised)",
                vaddr
            );
        });
        let (base, len) = if rc == 0 && !vaddr.is_null() {
            (vaddr.cast::<u8>(), self.stride as usize * self.height as usize * 4)
        } else if !self.mapping.is_null() {
            (self.mapping, self.mapping_len)
        } else {
            return;
        };
        {
            let pixels = std::slice::from_raw_parts_mut(base, len);
            let stride_bytes = self.stride as usize * 4;
            let band = (frame / 4) % 64;
            let bar = (frame as usize * 16) % self.width.max(1) as usize;
            for y in 0..self.height as usize {
                let row = &mut pixels[y * stride_bytes..y * stride_bytes + self.width as usize * 4];
                for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                    let x = x as u32;
                    let shift = ((x / 120 + y as u32 / 120 + band) & 1) * 96;
                    let in_bar = x as usize >= bar && (x as usize) < bar + 32;
                    pixel[0] = if in_bar { 255 } else { (x % 256) as u8 }; // B
                    pixel[1] = if in_bar { 255 } else { (y as u32 % 256) as u8 }; // G
                    pixel[2] = if in_bar { 255 } else { (96 + shift) as u8 }; // R
                    pixel[3] = 255; // A
                }
            }
        }
        if rc == 0 && !vaddr.is_null() {
            let _ = (self.api.unlock)(self.handle);
        }
    }
}

impl Drop for NativeBuffer {
    fn drop(&mut self) {
        unsafe {
            if !self.mapping.is_null() {
                libc::munmap(self.mapping.cast(), self.mapping_len);
            }
            (self.api.release)(self.handle, 1);
            (self.api.deinitialize)();
        }
    }
}

unsafe fn gralloc_api() -> Result<GrallocApi, String> {
    let name = std::ffi::CString::new("libgralloc.so.1").unwrap();
    let handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if handle.is_null() {
        let error = dl_error();
        return Err(format!("cannot load libgralloc.so.1: {error}"));
    }
    Ok(GrallocApi {
        initialize: resolve(handle, "hybris_gralloc_initialize")?,
        allocate: resolve(handle, "hybris_gralloc_allocate")?,
        release: resolve(handle, "hybris_gralloc_release")?,
        deinitialize: resolve(handle, "hybris_gralloc_deinitialize")?,
        lock: resolve(handle, "hybris_gralloc_lock")?,
        unlock: resolve(handle, "hybris_gralloc_unlock")?,
    })
}

fn dl_error() -> String {
    unsafe {
        let error = libc::dlerror();
        if error.is_null() {
            "unknown dlopen error".to_string()
        } else {
            std::ffi::CStr::from_ptr(error).to_string_lossy().into_owned()
        }
    }
}

unsafe fn resolve<T: Copy>(handle: *mut c_void, symbol: &str) -> Result<T, String> {
    let name = std::ffi::CString::new(symbol).unwrap();
    libc::dlerror();
    let pointer = libc::dlsym(handle, name.as_ptr());
    if pointer.is_null() {
        return Err(format!("{symbol} is unavailable: {}", dl_error()));
    }
    if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
        return Err(format!("{symbol}: unexpected function pointer size"));
    }
    Ok(std::mem::transmute_copy(&pointer))
}


// ---------------------------------------------------------------------------
// EGL/GLES: render the frame into the same gralloc allocation, on the GPU
// ---------------------------------------------------------------------------

const EGL_TRUE: i32 = 1;
const EGL_NONE: i32 = 0x3038;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_NATIVE_BUFFER_ANDROID: i32 = 0x3140;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;

type EglGetDisplay = unsafe extern "C" fn(usize) -> *mut c_void;
type EglInitialize = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> u32;
type EglChooseConfig =
    unsafe extern "C" fn(*mut c_void, *const i32, *mut *mut c_void, i32, *mut i32) -> u32;
type EglCreateContext =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const i32) -> *mut c_void;
type EglMakeCurrent =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> u32;
type EglCreateImageKHR = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    u32,
    *mut c_void,
    *const i32,
) -> *mut c_void;
type EglDestroyImageKHR = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type EglGetProcAddress = unsafe extern "C" fn(*const std::ffi::c_char) -> *mut c_void;
type EglGetError = unsafe extern "C" fn() -> i32;

type GlGenTextures = unsafe extern "C" fn(i32, *mut u32);
type GlBindTexture = unsafe extern "C" fn(u32, u32);
type GlGenFramebuffers = unsafe extern "C" fn(i32, *mut u32);
type GlBindFramebuffer = unsafe extern "C" fn(u32, u32);
type GlFramebufferTexture2D = unsafe extern "C" fn(u32, u32, u32, u32, i32);
type GlCheckFramebufferStatus = unsafe extern "C" fn(u32) -> u32;
type GlEGLImageTargetTexture2D = unsafe extern "C" fn(u32, *mut c_void);
type GlGetError = unsafe extern "C" fn() -> u32;
type GlViewport = unsafe extern "C" fn(i32, i32, i32, i32);
type GlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type GlClear = unsafe extern "C" fn(u32);
type GlFinish = unsafe extern "C" fn();

/// The subset of Android's `ANativeWindowBuffer` layout libhybris expects at
/// `EGL_NATIVE_BUFFER_ANDROID`. Same layout as the gleshim gralloc path.
#[repr(C)]
struct NativeWindowBuffer {
    magic: i32,
    version: i32,
    reserved: [*mut c_void; 4],
    inc_ref: *mut c_void,
    dec_ref: *mut c_void,
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    usage_deprecated: i32,
    layer_count: usize,
    reserved_ptr: *mut c_void,
    handle: *const c_void,
    usage: u64,
    reserved_proc: [*mut c_void; 7],
}

extern "C" fn noop_ref(_: *mut c_void) {}

struct EglApi {
    get_display: EglGetDisplay,
    initialize: EglInitialize,
    choose_config: EglChooseConfig,
    create_context: EglCreateContext,
    make_current: EglMakeCurrent,
    create_image: EglCreateImageKHR,
    destroy_image: EglDestroyImageKHR,
    get_error: EglGetError,
}

struct GlApi {
    gen_textures: GlGenTextures,
    bind_texture: GlBindTexture,
    gen_framebuffers: GlGenFramebuffers,
    bind_framebuffer: GlBindFramebuffer,
    framebuffer_texture_2d: GlFramebufferTexture2D,
    check_framebuffer_status: GlCheckFramebufferStatus,
    image_target_texture_2d: GlEGLImageTargetTexture2D,
    get_error: GlGetError,
    viewport: GlViewport,
    clear_color: GlClearColor,
    clear: GlClear,
    finish: GlFinish,
}

struct EglContext {
    display: *mut c_void,
    context: *mut c_void,
    egl: EglApi,
    gl: GlApi,
}

impl EglContext {
    /// An offscreen (surfaceless) ES2 context on the host driver. Lipstick and
    /// the guests use this same libEGL; no host window is needed because every
    /// frame is rendered into an FBO over a gralloc texture.
    unsafe fn create() -> Result<EglContext, String> {
        let egl_handle = dlopen_any(&["libEGL.so.1", "libEGL.so"])?;
        let gl_handle = dlopen_any(&["libGLESv2.so.2", "libGLESv2.so"])?;
        if std::env::var_os("PENGUDROID_WLEGL_TRACE").is_some() {
            eprintln!("[wlegl:egl] libEGL handle={egl_handle:p} libGLESv2 handle={gl_handle:p}");
        }
        let get_proc: EglGetProcAddress = resolve(egl_handle, "eglGetProcAddress")?;
        let egl = EglApi {
            get_display: resolve(egl_handle, "eglGetDisplay")?,
            initialize: resolve(egl_handle, "eglInitialize")?,
            choose_config: resolve(egl_handle, "eglChooseConfig")?,
            create_context: resolve(egl_handle, "eglCreateContext")?,
            make_current: resolve(egl_handle, "eglMakeCurrent")?,
            // libhybris exposes the image entry points only through
            // eglGetProcAddress: dlsym alone fails with "undefined symbol:
            // eglCreateImageKHR" on the Aurora driver.
            create_image: {
                let pointer = lookup(get_proc, egl_handle, "eglCreateImageKHR");
                if pointer.is_null() {
                    return Err("eglCreateImageKHR is missing (dlsym + eglGetProcAddress)".into());
                }
                eprintln!("[wlegl:egl] eglCreateImageKHR at {pointer:p}");
                std::mem::transmute_copy(&pointer)
            },
            destroy_image: {
                let pointer = lookup(get_proc, egl_handle, "eglDestroyImageKHR");
                if pointer.is_null() {
                    return Err("eglDestroyImageKHR is missing".into());
                }
                std::mem::transmute_copy(&pointer)
            },
            get_error: resolve(egl_handle, "eglGetError")?,
        };
        let gl = GlApi {
            gen_textures: resolve(gl_handle, "glGenTextures")?,
            bind_texture: resolve(gl_handle, "glBindTexture")?,
            gen_framebuffers: resolve(gl_handle, "glGenFramebuffers")?,
            bind_framebuffer: resolve(gl_handle, "glBindFramebuffer")?,
            framebuffer_texture_2d: resolve(gl_handle, "glFramebufferTexture2D")?,
            check_framebuffer_status: resolve(gl_handle, "glCheckFramebufferStatus")?,
            image_target_texture_2d: resolve_proc(get_proc, "glEGLImageTargetTexture2DOES")?,
            get_error: resolve(gl_handle, "glGetError")?,
            viewport: resolve(gl_handle, "glViewport")?,
            clear_color: resolve(gl_handle, "glClearColor")?,
            clear: resolve(gl_handle, "glClear")?,
            finish: resolve(gl_handle, "glFinish")?,
        };
        // EGL_DEFAULT_DISPLAY is 0. eglGetDisplay returns the platform display;
        // initializing it twice in one process is harmless.
        let display = (egl.get_display)(0);
        if display.is_null() {
            return Err("eglGetDisplay(EGL_DEFAULT_DISPLAY) returned null".into());
        }
        let (mut major, mut minor) = (0i32, 0i32);
        if (egl.initialize)(display, &mut major, &mut minor) != EGL_TRUE as u32 {
            return Err(format!("eglInitialize failed: {:#x}", (egl.get_error)()));
        }
        let attributes = [
            0x3033, 0x0001, // EGL_SURFACE_TYPE, EGL_PBUFFER_BIT
            0x3040, 0x0040, // EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT
            0x3024, 8, // EGL_RED_SIZE
            0x3023, 8, // EGL_GREEN_SIZE
            0x3022, 8, // EGL_BLUE_SIZE
            0x3021, 8, // EGL_ALPHA_SIZE
            EGL_NONE,
        ];
        let mut config = std::ptr::null_mut();
        let mut count = 0i32;
        if (egl.choose_config)(display, attributes.as_ptr(), &mut config, 1, &mut count)
            != EGL_TRUE as u32
            || count == 0
        {
            return Err("eglChooseConfig found no pbuffer ES2 RGBA8 config".into());
        }
        let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        let context = (egl.create_context)(
            display,
            config,
            std::ptr::null_mut(),
            context_attributes.as_ptr(),
        );
        if context.is_null() {
            return Err(format!("eglCreateContext failed: {:#x}", (egl.get_error)()));
        }
        if (egl.make_current)(display, std::ptr::null_mut(), std::ptr::null_mut(), context)
            != EGL_TRUE as u32
        {
            return Err(format!(
                "eglMakeCurrent(surfaceless) failed: {:#x}",
                (egl.get_error)()
            ));
        }
        println!("EGL {major}.{minor}: offscreen ES2 context ready");
        Ok(EglContext {
            display,
            context,
            egl,
            gl,
        })
    }

    /// Wrap a gralloc allocation as a texture-backed FBO: the GPU writes
    /// directly into the memory the compositor later samples.
    unsafe fn bind_native(&self, buffer: &NativeBuffer) -> Result<(u32, u32), String> {
        let mut native = NativeWindowBuffer {
            magic: i32::from_be_bytes(*b"_bfr"),
            version: std::mem::size_of::<NativeWindowBuffer>() as i32,
            reserved: [std::ptr::null_mut(); 4],
            inc_ref: noop_ref as *mut c_void,
            dec_ref: noop_ref as *mut c_void,
            width: buffer.width as i32,
            height: buffer.height as i32,
            stride: buffer.stride as i32,
            format: FORMAT_BGRA_8888,
            usage_deprecated: USAGE_LINEAR_TEXTURE as i32,
            layer_count: 1,
            reserved_ptr: std::ptr::null_mut(),
            handle: buffer.handle as *const c_void,
            usage: USAGE_LINEAR_TEXTURE as u64,
            reserved_proc: [std::ptr::null_mut(); 7],
        };
        let image = (self.egl.create_image)(
            self.display,
            std::ptr::null_mut(),
            EGL_NATIVE_BUFFER_ANDROID as u32,
            &mut native as *mut NativeWindowBuffer as *mut c_void,
            std::ptr::null(),
        );
        if image.is_null() {
            return Err(format!(
                "eglCreateImageKHR(EGL_NATIVE_BUFFER_ANDROID) failed: {:#x}",
                (self.egl.get_error)()
            ));
        }
        let mut texture = 0u32;
        let mut framebuffer = 0u32;
        (self.gl.gen_textures)(1, &mut texture);
        (self.gl.bind_texture)(0x0DE1, texture); // GL_TEXTURE_2D
        (self.gl.image_target_texture_2d)(0x0DE1, image);
        let gl_error = (self.gl.get_error)();
        (self.egl.destroy_image)(self.display, image);
        if gl_error != 0 {
            return Err(format!(
                "glEGLImageTargetTexture2DOES failed: GL error {gl_error:#x}"
            ));
        }
        (self.gl.gen_framebuffers)(1, &mut framebuffer);
        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
        (self.gl.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, 0x0DE1, texture, 0);
        let status = (self.gl.check_framebuffer_status)(GL_FRAMEBUFFER);
        if status != GL_FRAMEBUFFER_COMPLETE {
            return Err(format!("gralloc FBO is incomplete: {status:#x}"));
        }
        Ok((texture, framebuffer))
    }

    /// Clear the whole native buffer to a frame-dependent colour. This proves
    /// GPU presentation (and flips the colour, so orientation is visible too).
    unsafe fn clear_frame(&self, framebuffer: u32, frame: u32) {
        let phase = (frame as f32 / 90.0).sin();
        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
        (self.gl.clear_color)(0.2 + 0.6 * phase.abs(), 0.3, 0.7 - 0.5 * phase.abs(), 1.0);
        (self.gl.clear)(0x4000); // GL_COLOR_BUFFER_BIT
        (self.gl.finish)();
    }

    /// Invalidate the CPU view of the allocation after a GPU write, the same
    /// operation the CPU path performs on every frame. Without it the
    /// compositor may keep sampling the pixels it imported first.
    unsafe fn sync_cpu(&self, buffer: &NativeBuffer) {
        let mut vaddr: *mut c_void = std::ptr::null_mut();
        let rc = (buffer.api.lock)(
            buffer.handle,
            USAGE_LINEAR_TEXTURE as i32,
            0,
            0,
            buffer.width as i32,
            buffer.height as i32,
            &mut vaddr,
        );
        if rc == 0 {
            let _ = (buffer.api.unlock)(buffer.handle);
        }
    }
}

fn dlopen_any(names: &[&str]) -> Result<*mut c_void, String> {
    for name in names {
        let name = std::ffi::CString::new(*name).unwrap();
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if !handle.is_null() {
            return Ok(handle);
        }
    }
    Err(format!("cannot load any of {names:?}: {}", dl_error()))
}

/// Look up an EGL entry point: dlsym first, then eglGetProcAddress, because
/// libhybris exports the image/extension functions only via the latter.
/// Resolve an EGL symbol through dlsym, then eglGetProcAddress, reporting
/// where it was found so driver-specific quirks are visible in the probe log.
unsafe fn lookup(
    get_proc: EglGetProcAddress,
    handle: *mut c_void,
    symbol: &str,
) -> *mut c_void {
    let name = std::ffi::CString::new(symbol).unwrap();
    let direct = libc::dlsym(handle, name.as_ptr());
    if !direct.is_null() {
        eprintln!("[wlegl:egl] {symbol}: dlsym");
        return direct;
    }
    let via_proc = get_proc(name.as_ptr());
    eprintln!("[wlegl:egl] {symbol}: dlsym=null eglGetProcAddress={via_proc:p}");
    via_proc
}

unsafe fn resolve_egl<T: Copy>(
    handle: *mut c_void,
    get_proc: EglGetProcAddress,
    symbol: &str,
) -> Result<T, String> {
    match resolve(handle, symbol) {
        Ok(value) => return Ok(value),
        Err(error) => eprintln!("[wlegl:egl] dlsym failed: {error}"),
    }
    eprintln!("[wlegl:egl] trying eglGetProcAddress({symbol})");
    resolve_proc(get_proc, symbol)
}

unsafe fn resolve_proc<T: Copy>(get_proc: EglGetProcAddress, symbol: &str) -> Result<T, String> {
    let name = std::ffi::CString::new(symbol).unwrap();
    let pointer = get_proc(name.as_ptr());
    if pointer.is_null() {
        return Err(format!("{symbol} is not available from eglGetProcAddress"));
    }
    Ok(std::mem::transmute_copy(&pointer))
}

#[derive(Default)]
pub(crate) struct State {
    releases: u32,
    pings: u32,
}

macro_rules! empty_dispatch {
    ($interface:ty) => {
        impl Dispatch<$interface, ()> for State {
            fn event(
                _: &mut Self,
                _: &$interface,
                _: <$interface as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

empty_dispatch!(wl_compositor::WlCompositor);
// `registry_queue_init` keeps the global list itself, so the registry object
// only needs to accept the bookkeeping events it never delivers here.
impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: <wl_registry::WlRegistry as Proxy>::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
empty_dispatch!(wl_surface::WlSurface);

impl Dispatch<wl_shell::WlShell, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shell::WlShell,
        _: <wl_shell::WlShell as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shell_surface::WlShellSurface, ()> for State {
    fn event(
        state: &mut Self,
        shell_surface: &wl_shell_surface::WlShellSurface,
        event: <wl_shell_surface::WlShellSurface as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shell_surface::Event::Ping { serial } = event {
            state.pings += 1;
            shell_surface.pong(serial);
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: <wl_buffer::WlBuffer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.releases += 1;
        }
    }
}

impl Dispatch<AndroidWlegl, ()> for State {
    fn event(
        _: &mut Self,
        _: &AndroidWlegl,
        _: <AndroidWlegl as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<AndroidWleglHandle, ()> for State {
    fn event(
        _: &mut Self,
        _: &AndroidWleglHandle,
        _: <AndroidWleglHandle as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

struct Options {
    width: u32,
    height: u32,
    seconds: f32,
    hold: bool,
    /// Buffers in flight. One is never enough: the compositor keeps the buffer
    /// it displays and only releases it once another one replaces it.
    buffers: usize,
    interval_ms: u64,
    /// Build a fresh wl_buffer for every frame instead of reusing one per
    /// allocation. This distinguishes "the compositor re-samples the
    /// allocation" from "it samples once per wl_buffer import".
    reimport: bool,
    /// Present the same gralloc allocation through wl_shm (the current guest
    /// path) instead of android_wlegl, so both transports can be compared on
    /// one device and one allocation.
    shm: bool,
    /// Render each frame on the GPU into the presented gralloc allocation
    /// (EGLImage -> FBO) instead of writing pixels with the CPU.
    egl: bool,
    /// Test a real wl_egl_window + eglCreateWindowSurface + eglSwapBuffers
    /// instead of the manual android_wlegl buffer path.
    window: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut options = Options {
            width: 480,
            height: 800,
            seconds: 8.0,
            hold: false,
            buffers: 3,
            interval_ms: 16,
            reimport: false,
            shm: false,
            egl: false,
            window: false,
        };
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--width" => {
                    options.width = number(args.get(index + 1), "--width")?;
                    index += 2;
                }
                "--height" => {
                    options.height = number(args.get(index + 1), "--height")?;
                    index += 2;
                }
                "--seconds" => {
                    options.seconds = args
                        .get(index + 1)
                        .and_then(|value| value.parse().ok())
                        .ok_or("--seconds needs a number")?;
                    index += 2;
                }
                "--buffers" => {
                    options.buffers = number(args.get(index + 1), "--buffers")?.clamp(2, 8) as usize;
                    index += 2;
                }
                "--interval" => {
                    options.interval_ms = number(args.get(index + 1), "--interval")?.clamp(1, 1000) as u64;
                    index += 2;
                }
                "--egl" => {
                    options.egl = true;
                    index += 1;
                }
                "--window" => {
                    options.window = true;
                    index += 1;
                }
                "--shm" => {
                    options.shm = true;
                    index += 1;
                }
                "--reimport" => {
                    options.reimport = true;
                    index += 1;
                }
                "--hold" => {
                    options.hold = true;
                    index += 1;
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        options.width = options.width.clamp(64, 4096);
        options.height = options.height.clamp(64, 4096);
        Ok(options)
    }
}

fn number(value: Option<&String>, name: &str) -> Result<u32, String> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("{name} needs an integer"))
}

/// Run the probe. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    match probe(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("wlegl-probe: {error}");
            1
        }
    }
}

fn probe(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args)?;
    let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland display: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)
        .map_err(|e| format!("cannot read the Wayland registry: {e}"))?;
    let qh = queue.handle();

    let mut wlegl_name = None;
    let mut wlegl_version = 0;
    globals.contents().with_list(|list| {
        for global in list {
            match global.interface.as_str() {
                "android_wlegl" => {
                    wlegl_name = Some(global.name);
                    wlegl_version = global.version;
                }
                // Print the transport-relevant globals so the output doubles as
                // evidence of what this compositor offers.
                "zwp_linux_dmabuf_v1" | "wl_drm" | "wl_shm" | "wl_shell" | "wl_compositor"
                | "qt_hardware_integration" | "qt_surface_extension" => {
                    println!("global {} v{}", global.interface, global.version);
                }
                _ => {}
            }
        }
    });

    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=4, ())
        .map_err(|e| format!("wl_compositor unavailable: {e}"))?;
    let shell: wl_shell::WlShell = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_shell unavailable (needed for the Aurora window role): {e}"))?;
    let surface = compositor.create_surface(&qh, ());
    let shell_surface = shell.get_shell_surface(&surface, &qh, ());
    shell_surface.set_toplevel();
    shell_surface.set_title(if options.window {
        "PenguDroid EGL red-window probe".to_string()
    } else {
        "PenguDroid android_wlegl probe".to_string()
    });
    shell_surface.set_class("io.github.denizomer10.PenguDroid.wlegl-probe".to_string());

    // The EGL window test deliberately has no dependency on wl_shm,
    // android_wlegl, or gralloc. Keep it ahead of all native-buffer setup so a
    // failure in those unrelated paths cannot prevent this basic test.
    if options.window {
        return unsafe { run_window_test(&conn, &mut queue, &surface, &options) };
    }

    let wlegl = if options.shm {
        None
    } else {
        let Some(wlegl_name) = wlegl_name else {
            return Err(
                "the compositor does not advertise android_wlegl; this transport is unavailable"
                    .into(),
            );
        };
        println!("android_wlegl: global {wlegl_name} v{wlegl_version}");
        Some(
            globals
                .registry()
                .bind(wlegl_name, wlegl_version.min(1), &qh, ()),
        )
    };

    let mut buffers = Vec::with_capacity(options.buffers);
    for _ in 0..options.buffers {
        buffers.push(unsafe { NativeBuffer::allocate(options.width, options.height)? });
    }
    let first = &buffers[0];
    println!(
        "gralloc: {}x{} stride={} format={FORMAT_BGRA_8888} usage={USAGE_LINEAR_TEXTURE:#x} fds={} ints={} bytes, {} buffers",
        first.width,
        first.height,
        first.stride,
        first.fds.len(),
        first.ints.len(),
        buffers.len()
    );

    // The compositor keeps whatever buffer it is displaying and releases it
    // only once another buffer replaces it, so a single buffer can never
    // animate: present round-robin from a small pool and count releases.
    // EGL lives after the Wayland connection exists: the probe's whole point
    // is that both the GPU and the compositor refer to one native allocation.
    let mut egl_targets: Vec<(u32, u32)> = Vec::new();
    let egl_context = if options.egl {
        let context = unsafe { EglContext::create()? };
        for buffer in &buffers {
            egl_targets.push(unsafe { context.bind_native(buffer)? });
        }
        println!("EGL: {} gralloc FBOs ready (GPU rendering)", egl_targets.len());
        Some(context)
    } else {
        None
    };

    let mut wl_buffers = Vec::with_capacity(buffers.len());
    if options.shm {
        let shm = crate::wl_shm::bind(&globals, &qh)?;
        for buffer in &buffers {
            wl_buffers.push(crate::wl_shm::create_buffer(
                &shm,
                buffer.shm_fd,
                buffer.width,
                buffer.height,
                buffer.stride,
                &qh,
            )?);
        }
        println!(
            "wl_shm: created {} buffers from the same gralloc allocation",
            wl_buffers.len()
        );
    } else {
        let wlegl = wlegl.as_ref().expect("bound above for the wlegl path");
        for buffer in &buffers {
            wl_buffers.push(create_buffer(wlegl, buffer, &qh)?);
        }
        println!(
            "android_wlegl: created {} wl_buffers from the native handles",
            wl_buffers.len()
        );
    }

    let started = Instant::now();
    // `--hold` keeps the window up until the process is killed, which is what a
    // screenshot run needs; otherwise the probe stops after `--seconds`.
    let budget = if options.hold {
        Duration::from_secs(u32::MAX as u64)
    } else {
        Duration::from_secs_f32(options.seconds.max(1.0))
    };
    let interval = Duration::from_millis(options.interval_ms);
    let mut state = State::default();
    let mut frame = 0u32;
    let mut presented = 0u32;
    let mut last_report = Instant::now();
    let mut reported_presents = 0u32;
    let mut next_present = Instant::now();

    while started.elapsed() < budget {
        let now = Instant::now();
        if now >= next_present {
            let index = frame as usize % buffers.len();
            match (&egl_context, egl_targets.get(index)) {
                (Some(context), Some(&(_, framebuffer))) => unsafe {
                    context.clear_frame(framebuffer, frame);
                    context.sync_cpu(&buffers[index]);
                },
                _ => unsafe { buffers[index].paint(frame) },
            }
            if options.reimport && !options.shm {
                // A new wl_buffer forces a new import on the compositor side.
                let fresh = create_buffer(
                    wlegl.as_ref().expect("bound above for the wlegl path"),
                    &buffers[index],
                    &qh,
                )?;
                present_frame(&surface, &fresh, options.width, options.height)?;
                wl_buffers[index] = fresh;
            } else {
                present_frame(&surface, &wl_buffers[index], options.width, options.height)?;
            }
            conn.flush().map_err(|e| e.to_string())?;
            frame += 1;
            presented += 1;
            next_present += interval;
            if next_present < now {
                next_present = now + interval;
            }
        }
        // Never block: a compositor that stops releasing must show up as
        // releases=0 in the report, not as a hung probe.
        let wait_ms = next_present
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(16) as i32;
        dispatch_with_timeout(&conn, &mut queue, &mut state, wait_ms)?;
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!(
                "presented={presented} releases={} pings={} ({:.1} fps)",
                state.releases,
                state.pings,
                (presented - reported_presents) as f32 / last_report.elapsed().as_secs_f32()
            );
            reported_presents = presented;
            last_report = Instant::now();
        }
    }

    println!(
        "done: {presented} frames in {:.1}s, releases={}, pings={}",
        started.elapsed().as_secs_f32(),
        state.releases,
        state.pings
    );
    if state.releases == 0 {
        println!(
            "verdict: the compositor never released a buffer. The wl_buffers were \
             accepted without a protocol error, but nothing proves they were sampled."
        );
    } else if state.releases < presented {
        println!(
            "verdict: accepted and sampled; {} of {presented} presents were released \
             (the compositor drops or backlog-frames the rest)",
            state.releases
        );
    } else {
        println!(
            "verdict: the compositor sampled every android_wlegl buffer and released it"
        );
    }
    drop(wl_buffers);
    // wl_shell_surface has no destructor request; it dies with the surface.
    surface.destroy();
    conn.flush().map_err(|e| e.to_string())?;
    Ok(())
}


/// Attach the buffer, damage the whole surface and commit one frame.
fn present_frame(
    surface: &wl_surface::WlSurface,
    buffer: &wl_buffer::WlBuffer,
    width: u32,
    height: u32,
) -> Result<(), String> {
    surface.attach(Some(buffer), 0, 0);
    surface.damage(0, 0, width as i32, height as i32);
    surface.commit();
    Ok(())
}

/// Dispatch Wayland events, waiting at most `timeout_ms` for the compositor.
///
/// `Queue::blocking_dispatch` waits without a timeout, so a compositor that
/// stops releasing buffers would hang the probe instead of reporting it.
fn dispatch_with_timeout(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    timeout_ms: i32,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    if let Some(guard) = conn.prepare_read() {
        let fd = guard.connection_fd().as_raw_fd();
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll_fd` is a valid single-entry poll array for the duration
        // of the call.
        unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if guard.read().is_ok() {
            queue
                .dispatch_pending(state)
                .map_err(|e| format!("Wayland dispatch failed: {e}"))?;
        }
    } else {
        queue
            .dispatch_pending(state)
            .map_err(|e| format!("Wayland dispatch failed: {e}"))?;
    }
    Ok(())
}

fn create_buffer(
    wlegl: &AndroidWlegl,
    buffer: &NativeBuffer,
    qh: &QueueHandle<State>,
) -> Result<wl_buffer::WlBuffer, String> {
    // The ints are sent first with the expected fd count, then each fd.
    let handle = wlegl.create_handle(buffer.fds.len() as i32, buffer.ints.clone(), qh, ());
    for &fd in &buffer.fds {
        if fd < 0 {
            return Err(format!("gralloc handle contains an invalid fd {fd}"));
        }
        // SAFETY: the fds stay open for the lifetime of the gralloc handle,
        // which outlives this request; the protocol duplicates them on send.
        handle.add_fd(unsafe { BorrowedFd::borrow_raw(fd) });
    }
    let wl_buffer = wlegl.create_buffer(
        buffer.width as i32,
        buffer.height as i32,
        buffer.stride as i32,
        FORMAT_BGRA_8888,
        USAGE_LINEAR_TEXTURE as i32,
        &handle,
        qh,
        (),
    );
    handle.destroy();
    Ok(wl_buffer)
}

/// Isolated red-window check: wl_shell + wl_egl_window + EGL/GLES only.
/// No wl_shm pool, android_wlegl handle, gralloc allocation, or CPU pixel
/// buffer is involved. eglSwapBuffers attaches the GPU-rendered frame directly
/// to the wl_surface.
unsafe fn run_window_test(
    conn: &wayland_client::Connection,
    queue: &mut wayland_client::EventQueue<State>,
    surface: &wl_surface::WlSurface,
    options: &Options,
) -> Result<(), String> {
    type GetPlatformDisplay =
        unsafe extern "C" fn(u32, *mut c_void, *const isize) -> *mut c_void;
    type BindApi = unsafe extern "C" fn(u32) -> u32;
    type CreateWindowSurface = unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *const i32,
    ) -> *mut c_void;
    type SwapBuffers = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
    type DestroySurface = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
    type DestroyContext = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
    type Terminate = unsafe extern "C" fn(*mut c_void) -> u32;

    let width = options.width;
    let height = options.height;
    println!("window probe: creating a {width}x{height} red EGL window (no wl_shm)");
    // Establish the wl_shell role before the EGL driver attaches its first
    // buffer. This is an empty commit, not a wl_shm presentation.
    surface.commit();
    conn.flush().map_err(|e| format!("initial Wayland flush: {e}"))?;

    let window = wayland_egl::WlEglSurface::new(surface.id(), width as i32, height as i32)
        .map_err(|e| format!("wl_egl_window_create: {e:?}"))?;
    println!("window probe: wl_egl_window ptr={:p}", window.ptr());

    let egl = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if egl.is_null() {
        return Err(format!("libEGL unavailable: {}", dl_error()));
    }
    let gles = libc::dlopen(c"libGLESv2.so.2".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if gles.is_null() {
        return Err(format!("libGLESv2 unavailable: {}", dl_error()));
    }

    let get_display: GetPlatformDisplay = resolve(egl, "eglGetPlatformDisplay")?;
    let get_error: EglGetError = resolve(egl, "eglGetError")?;
    let initialize: EglInitialize = resolve(egl, "eglInitialize")?;
    let bind_api: BindApi = resolve(egl, "eglBindAPI")?;
    let choose_config: EglChooseConfig = resolve(egl, "eglChooseConfig")?;
    let create_window: CreateWindowSurface = resolve(egl, "eglCreateWindowSurface")?;
    let create_context: EglCreateContext = resolve(egl, "eglCreateContext")?;
    let make_current: EglMakeCurrent = resolve(egl, "eglMakeCurrent")?;
    let swap_buffers: SwapBuffers = resolve(egl, "eglSwapBuffers")?;
    let destroy_surface: DestroySurface = resolve(egl, "eglDestroySurface")?;
    let destroy_context: DestroyContext = resolve(egl, "eglDestroyContext")?;
    let terminate: Terminate = resolve(egl, "eglTerminate")?;
    let viewport: GlViewport = resolve(gles, "glViewport")?;
    let clear_color: GlClearColor = resolve(gles, "glClearColor")?;
    let clear: GlClear = resolve(gles, "glClear")?;

    let display = get_display(
        0x31D8, // EGL_PLATFORM_WAYLAND
        conn.display().id().as_ptr() as *mut c_void,
        std::ptr::null(),
    );
    if display.is_null() {
        return Err(format!("eglGetPlatformDisplay returned null: {:#x}", get_error()));
    }
    println!("window probe: eglGetPlatformDisplay -> {display:p}");
    let (mut major, mut minor) = (0, 0);
    if initialize(display, &mut major, &mut minor) != 1 {
        return Err(format!("eglInitialize failed: {:#x}", get_error()));
    }
    println!("window probe: EGL {major}.{minor} initialized");
    if bind_api(0x30A0) != 1 {
        return Err(format!("eglBindAPI(EGL_OPENGL_ES_API) failed: {:#x}", get_error()));
    }

    let attributes = [
        0x3033, 0x0004, // EGL_SURFACE_TYPE, EGL_WINDOW_BIT
        0x3040, 0x0004, // EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT
        0x3024, 8, // EGL_RED_SIZE
        0x3023, 8, // EGL_GREEN_SIZE
        0x3022, 8, // EGL_BLUE_SIZE
        0x3021, 8, // EGL_ALPHA_SIZE
        EGL_NONE,
    ];
    let mut config = std::ptr::null_mut();
    let mut count = 0;
    if choose_config(display, attributes.as_ptr(), &mut config, 1, &mut count) != 1 || count < 1 {
        return Err(format!("eglChooseConfig found no window ES2 config: {:#x}", get_error()));
    }
    let egl_surface = create_window(display, config, window.ptr() as *mut c_void, std::ptr::null());
    if egl_surface.is_null() {
        return Err(format!("eglCreateWindowSurface failed: {:#x}", get_error()));
    }
    println!("window probe: eglCreateWindowSurface -> {egl_surface:p}");

    let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
    let context = create_context(
        display,
        config,
        std::ptr::null_mut(),
        context_attributes.as_ptr(),
    );
    if context.is_null() {
        return Err(format!("eglCreateContext failed: {:#x}", get_error()));
    }
    if make_current(display, egl_surface, egl_surface, context) != 1 {
        return Err(format!("eglMakeCurrent failed: {:#x}", get_error()));
    }
    viewport(0, 0, width as i32, height as i32);
    clear_color(1.0, 0.0, 0.0, 1.0);
    println!("window probe: ES2 context ready; presenting solid red frames");

    let started = Instant::now();
    let budget = if options.hold {
        Duration::from_secs(u32::MAX as u64)
    } else {
        Duration::from_secs_f32(options.seconds.max(1.0))
    };
    let interval = Duration::from_millis(options.interval_ms);
    let mut next_present = Instant::now();
    let mut frames = 0u64;
    let mut state = State::default();
    while started.elapsed() < budget {
        let now = Instant::now();
        if now >= next_present {
            clear(0x4000); // GL_COLOR_BUFFER_BIT
            if swap_buffers(display, egl_surface) != 1 {
                return Err(format!(
                    "eglSwapBuffers failed on frame {}: {:#x}",
                    frames + 1,
                    get_error()
                ));
            }
            conn.flush().map_err(|e| format!("Wayland flush after swap: {e}"))?;
            frames += 1;
            if frames == 1 {
                println!("window probe: SUCCESS (first red frame swapped)");
            }
            next_present += interval;
            if next_present < now {
                next_present = now + interval;
            }
        }
        let wait_ms = next_present
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(16) as i32;
        dispatch_with_timeout(conn, queue, &mut state, wait_ms)?;
    }
    println!(
        "window probe: done, {frames} red frames in {:.1}s, pings={}",
        started.elapsed().as_secs_f32(),
        state.pings
    );

    make_current(
        display,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    destroy_surface(display, egl_surface);
    destroy_context(display, context);
    terminate(display);
    Ok(())
}
