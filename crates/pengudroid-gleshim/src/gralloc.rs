//! Native Hybris gralloc allocation, GPU flip and wl_shm presentation.
//!
//! This backend never handles EGL_MESA/EXT dma-buf export; that independent
//! implementation lives in `dmabuf.rs`.

use std::ffi::{CString, c_char, c_int, c_void};
use std::os::fd::RawFd;
use std::sync::{Condvar, Mutex, OnceLock};

use pengudroid_proto::abi::{ANDROID_RGBA_8888, HAL_BGRA_8888,
    NATIVE_FD_FLAG, WLEGL_FD_FLAG};
use pengudroid_proto::{BufferCrop, Message};
use pengudroid_socket::Connection;

/// `PresentShm`'s `native_window` field carries an Android native handle for
/// `android_wlegl` rather than a window id when it carries this marker.
const WLEGL_MAGIC: u64 = 0x776c_6567_6c00_0000;

/// `android_wlegl` needs the whole `native_handle_t`, not just one mmap-able
/// fd: `create_handle(num_fds, ints)` plus one `add_fd` per descriptor.
/// Keeping the two pieces next to the target makes both presentation paths
/// (wl_shm and android_wlegl) usable on the very same allocation.
#[derive(Clone)]
struct WleglHandle {
    fds: Vec<RawFd>,
    ints: Vec<i32>,
}

fn wlegl_handle(handle: *mut c_void) -> Option<WleglHandle> {
    if handle.is_null() {
        return None;
    }
    let header = handle as *const i32;
    let (num_fds, num_ints) = unsafe { (*header.add(1), *header.add(2)) };
    if !(1..=64).contains(&num_fds) || !(0..=1024).contains(&num_ints) {
        return None;
    }
    let mut fds = Vec::with_capacity(num_fds as usize);
    for index in 0..num_fds as usize {
        fds.push(unsafe { *header.add(3 + index) });
    }
    let mut ints = Vec::with_capacity(num_ints as usize);
    for index in 0..num_ints as usize {
        ints.push(unsafe { *header.add(3 + num_fds as usize + index) });
    }
    Some(WleglHandle { fds, ints })
}

/// True when the launcher opted into the compositor's native-buffer transport.
/// It needs no capability probe on the guest side: the daemon decides and
/// falls back to wl_shm if the compositor lacks `android_wlegl`.
pub(super) fn wlegl_requested() -> bool {
    env_truthy("PENGUDROID_GRALLOC_WLEGL")
}

/// The daemon attaches the native buffer to Lipstick directly (the ARM32
/// transport) instead of sampling it through its own EGL presenter. The
/// compositor shows the buffer as is, so the guest must deliver it upright:
/// the Y-flip that the EGL presenter path skips has to happen here.
pub(super) fn wlegl_buffer_requested() -> bool {
    wlegl_requested() && env_truthy("PENGUDROID_WLEGL_BUFFER")
}

/// WLEGL uses fenced async presentation by default. The legacy SHM path keeps
/// its historical opt-in; an explicit 0/false disables either async path.
pub(super) fn async_requested() -> bool {
    std::env::var("PENGUDROID_GRALLOC_ASYNC").map(|value| {
        let value = value.trim();
        value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
    }).unwrap_or_else(|_| wlegl_requested())
}

#[cfg(target_arch = "aarch64")]
use super::mapped_elf_base;
use super::{
    CURRENT_CONTEXT, EGL_CONDITION_SATISFIED_KHR, EGLClientBuffer, EGLDisplay, EGLSurface, EGLSync,
    FRAME_TRACE_STATE, GL_COLOR_ATTACHMENT0, GL_COLOR_BUFFER_BIT, GL_FRAMEBUFFER,
    GL_FRAMEBUFFER_COMPLETE, GL_NEAREST, GL_SCISSOR_TEST, GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER,
    GL_TEXTURE_MIN_FILTER, Host, SurfaceRec, cast_fn, close_fds, context_info, ensure_surface_id,
    env_truthy, host, host_error, host_get_integer, hybris_host, hybris_tls, input_trace_now_ms,
    lock_state, notify_frame_presented_vsync, pace_swap, sym_ptr,
};

pub(super) struct GrallocTarget {
    pub(super) handle: *mut c_void,
    pub(super) fd: RawFd,
    pub(super) stride: u32,
    /// Full native handle, used when the daemon presents via android_wlegl.
    wlegl: Option<WleglHandle>,
}

pub(super) struct GrallocFlipBuffer {
    pub(super) target: GrallocTarget,
    pub(super) fbo: u32,
}

pub(super) struct GrallocFlip {
    pub(super) buffers: Vec<GrallocFlipBuffer>,
    pub(super) next: usize,
    pub(super) last: usize,
}

type ShaderSource = unsafe extern "C" fn(u32, i32, *const *const c_char, *const i32);
type ShaderIv = unsafe extern "C" fn(u32, u32, *mut i32);
type ShaderInfoLog = unsafe extern "C" fn(u32, i32, *mut i32, *mut c_char);
type Uniform1i = unsafe extern "C" fn(i32, i32);
type GenObjects = unsafe extern "C" fn(i32, *mut u32);
type BindObject = unsafe extern "C" fn(u32);

#[derive(Clone, Copy)]
struct ShaderApi {
    create_shader: unsafe extern "C" fn(u32) -> u32,
    shader_source: ShaderSource,
    compile_shader: BindObject,
    get_shader_iv: ShaderIv,
    shader_info_log: ShaderInfoLog,
    create_program: unsafe extern "C" fn() -> u32,
    attach_shader: unsafe extern "C" fn(u32, u32),
    link_program: BindObject,
    get_program_iv: ShaderIv,
    program_info_log: ShaderInfoLog,
    use_program: BindObject,
    get_uniform_location: unsafe extern "C" fn(u32, *const c_char) -> i32,
    uniform1i: Uniform1i,
    gen_vao: GenObjects,
    bind_vao: BindObject,
    draw_arrays: unsafe extern "C" fn(u32, i32, i32),
    active_texture: BindObject,
}

#[derive(Clone, Copy)]
struct ShaderFlipState {
    api: ShaderApi,
    program: u32,
    vao: u32,
}

unsafe fn resolve_shader_proc(host: &Host, name: &str) -> Option<*mut c_void> {
    let get = host.get_proc_address?;
    let name = CString::new(name).ok()?;
    let pointer = get(name.as_ptr());
    if pointer.is_null() {
        sym_ptr(host.gles_handle as *mut c_void, name.to_str().ok()?)
    } else {
        Some(pointer)
    }
}

unsafe fn load_shader_api(host: &Host) -> Result<ShaderApi, String> {
    macro_rules! load {
        ($name:literal, $ty:ty) => {
            resolve_shader_proc(host, $name)
                .and_then(cast_fn::<$ty>)
                .ok_or_else(|| format!("{} is unavailable", $name))?
        };
    }
    Ok(ShaderApi {
        create_shader: load!("glCreateShader", unsafe extern "C" fn(u32) -> u32),
        shader_source: load!("glShaderSource", ShaderSource),
        compile_shader: load!("glCompileShader", BindObject),
        get_shader_iv: load!("glGetShaderiv", ShaderIv),
        shader_info_log: load!("glGetShaderInfoLog", ShaderInfoLog),
        create_program: load!("glCreateProgram", unsafe extern "C" fn() -> u32),
        attach_shader: load!("glAttachShader", unsafe extern "C" fn(u32, u32)),
        link_program: load!("glLinkProgram", BindObject),
        get_program_iv: load!("glGetProgramiv", ShaderIv),
        program_info_log: load!("glGetProgramInfoLog", ShaderInfoLog),
        use_program: load!("glUseProgram", BindObject),
        get_uniform_location: load!(
            "glGetUniformLocation",
            unsafe extern "C" fn(u32, *const c_char) -> i32
        ),
        uniform1i: load!("glUniform1i", Uniform1i),
        gen_vao: load!("glGenVertexArrays", GenObjects),
        bind_vao: load!("glBindVertexArray", BindObject),
        draw_arrays: load!("glDrawArrays", unsafe extern "C" fn(u32, i32, i32)),
        active_texture: load!("glActiveTexture", BindObject),
    })
}

unsafe fn shader_info(api: &ShaderApi, shader: u32, program: bool) -> String {
    let mut bytes = vec![0u8; 2048];
    let mut length = 0i32;
    if program {
        (api.program_info_log)(
            shader,
            bytes.len() as i32,
            &mut length,
            bytes.as_mut_ptr() as *mut c_char,
        );
    } else {
        (api.shader_info_log)(
            shader,
            bytes.len() as i32,
            &mut length,
            bytes.as_mut_ptr() as *mut c_char,
        );
    }
    String::from_utf8_lossy(&bytes[..length.max(0) as usize]).into_owned()
}

unsafe fn compile_flip_shader(api: &ShaderApi, kind: u32, source: &str) -> Result<u32, String> {
    let shader = (api.create_shader)(kind);
    if shader == 0 {
        return Err("glCreateShader returned 0".into());
    }
    let source = CString::new(source).unwrap();
    let pointer = source.as_ptr();
    (api.shader_source)(shader, 1, &pointer, std::ptr::null());
    (api.compile_shader)(shader);
    let mut ok = 0;
    (api.get_shader_iv)(shader, 0x8B81, &mut ok); // GL_COMPILE_STATUS
    if ok == 0 {
        return Err(format!(
            "shader compilation failed: {}",
            shader_info(api, shader, false)
        ));
    }
    Ok(shader)
}

unsafe fn create_shader_flip_state(host: &Host) -> Result<ShaderFlipState, String> {
    let started = std::time::Instant::now();
    let api = load_shader_api(host)?;
    let vertex = compile_flip_shader(
        &api,
        0x8B31,
        "#version 300 es
out vec2 v_uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = vec2(p.x, 1.0 - p.y);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}",
    )?;
    let fragment = compile_flip_shader(
        &api,
        0x8B30,
        "#version 300 es
precision mediump float;
in vec2 v_uv;
uniform sampler2D u_texture;
out vec4 out_color;
void main() { out_color = texture(u_texture, v_uv); }",
    )?;
    let program = (api.create_program)();
    (api.attach_shader)(program, vertex);
    (api.attach_shader)(program, fragment);
    (api.link_program)(program);
    let mut ok = 0;
    (api.get_program_iv)(program, 0x8B82, &mut ok); // GL_LINK_STATUS
    if ok == 0 {
        return Err(format!(
            "shader link failed: {}",
            shader_info(&api, program, true)
        ));
    }
    let mut vao = 0;
    (api.gen_vao)(1, &mut vao);
    if vao == 0 {
        return Err("glGenVertexArrays returned 0".into());
    }
    let location = (api.get_uniform_location)(program, c"u_texture".as_ptr());
    (api.use_program)(program);
    (api.uniform1i)(location, 0);
    if std::env::var_os("PENGUDROID_PROFILE_GRALLOC").is_some() {
        eprintln!(
            "[gleshim:gralloc:setup] shader flip program={:.2}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(ShaderFlipState { api, program, vao })
}

fn shader_flip_state(host: &Host, serial: u64) -> Option<ShaderFlipState> {
    static STATES: OnceLock<Mutex<std::collections::HashMap<u64, Option<ShaderFlipState>>>> =
        OnceLock::new();
    let states = STATES.get_or_init(Default::default);
    if let Some(state) = states
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&serial)
        .copied()
    {
        return state;
    }
    let state = unsafe { create_shader_flip_state(host) }
        .map_err(|error| {
            eprintln!("[gleshim:gralloc] shader flip unavailable, using blit fallback: {error}");
            error
        })
        .ok();
    states
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(serial, state);
    state
}

unsafe fn shader_flip_draw(
    host: &Host,
    serial: u64,
    texture: u32,
    width: u32,
    height: u32,
) -> bool {
    let Some(state) = shader_flip_state(host, serial) else {
        return false;
    };
    let api = state.api;
    let previous_program = host_get_integer(host, 0x8B8D) as u32; // GL_CURRENT_PROGRAM
    let previous_vao = host_get_integer(host, 0x85B5) as u32; // GL_VERTEX_ARRAY_BINDING
    let previous_texture = host_get_integer(host, 0x8069) as u32; // GL_TEXTURE_BINDING_2D
    let previous_active = host_get_integer(host, 0x84E0) as u32; // GL_ACTIVE_TEXTURE
    let mut viewport = [0i32; 4];
    if let Some(get) = host.gl_get_integerv {
        get(0x0BA2, viewport.as_mut_ptr());
    }
    let toggles = [0x0B71, 0x0B90, 0x0C11, 0x0BE2, 0x0B44]; // depth/stencil/scissor/blend/cull
    let mut enabled = [false; 5];
    if let (Some(is_enabled), Some(disable)) = (host.gl_is_enabled, host.gl_disable) {
        for (index, capability) in toggles.iter().enumerate() {
            enabled[index] = is_enabled(*capability) != 0;
            disable(*capability);
        }
    }
    (api.active_texture)(0x84C0); // GL_TEXTURE0
    if let Some(bind) = host.gl_bind_texture {
        bind(GL_TEXTURE_2D, texture);
    }
    (api.use_program)(state.program);
    (api.bind_vao)(state.vao);
    if let Some(set) = host.gl_viewport {
        set(0, 0, width as i32, height as i32);
    }
    (api.draw_arrays)(0x0004, 0, 3); // GL_TRIANGLES
    (api.use_program)(previous_program);
    (api.bind_vao)(previous_vao);
    if let Some(bind) = host.gl_bind_texture {
        bind(GL_TEXTURE_2D, previous_texture);
    }
    (api.active_texture)(previous_active);
    if let Some(set) = host.gl_viewport {
        set(viewport[0], viewport[1], viewport[2], viewport[3]);
    }
    if let Some(enable) = host.gl_enable {
        for (index, capability) in toggles.iter().enumerate() {
            if enabled[index] {
                enable(*capability);
            }
        }
    }
    true
}

/// Flip the native texture on the GPU because Lipstick's wl_shell ignores
/// wl_buffer transforms. No CPU readback is involved.
pub(super) unsafe fn flip_gralloc_surface(surface: EGLSurface, host: &Host) {
    if std::env::var_os("PENGUDROID_GRALLOC_SHM").is_none()
        || (wlegl_requested() && !async_requested() && !wlegl_buffer_requested())
    {
        return;
    }
    let rec = &mut *(surface as *mut SurfaceRec);
    let current = host
        .get_current_context
        .map(|f| f() as usize)
        .filter(|c| *c != 0)
        .unwrap_or_else(|| CURRENT_CONTEXT.with(|c| c.get()));
    let info = context_info(current);
    let Some(target) = rec
        .targets
        .iter_mut()
        .find(|target| target.group == info.group)
    else {
        return;
    };
    let Some(flip) = target.flip.as_mut() else {
        return;
    };
    let Some((_, read_fbo)) = target
        .fbos
        .iter()
        .find(|(serial, _)| *serial == info.serial)
    else {
        return;
    };
    let index = if flip.buffers.len() > 1 { flip.next } else { 0 };
    flip.last = index;
    flip.next = index ^ 1;
    let (read_fbo, write_fbo) = (*read_fbo, flip.buffers[index].fbo);
    let Some(bind) = host.gl_bind_framebuffer else {
        return;
    };
    bind(0x8CA8, read_fbo); // GL_READ_FRAMEBUFFER
    bind(0x8CA9, write_fbo); // GL_DRAW_FRAMEBUFFER
    let mut flipped = false;
    // WLEGL async uses this pass only to select/copy into an alternate output
    // buffer. The source is already upright, so a flipping shader is wrong.
    let shader_flip = std::env::var_os("PENGUDROID_GRALLOC_SHADER_FLIP").is_some()
        && !wlegl_requested();
    if shader_flip {
        flipped = shader_flip_draw(host, info.serial, target.tex, rec.w, rec.h);
    }
    if !flipped {
        let Some(blit) = host.gl_blit_framebuffer else {
            return;
        };
        // HWUI leaves its dirty-rectangle scissor enabled on some frames. The
        // flip is a separate full-surface operation, not part of that damage:
        // glBlitFramebuffer honors scissor, so leaving it enabled can preserve
        // stale pixels in the gralloc target (notably MAX's 420-dpi route).
        let scissor = host
            .gl_is_enabled
            .map(|is_enabled| is_enabled(GL_SCISSOR_TEST) != 0);
        if scissor == Some(true) {
            if let Some(disable) = host.gl_disable {
                disable(GL_SCISSOR_TEST);
            }
        }
        let (dst_y0, dst_y1) = if wlegl_buffer_requested() {
            (rec.h as i32, 0) // the compositor presents this buffer as is
        } else if wlegl_requested() {
            (0, rec.h as i32) // direct copy: no second orientation flip
        } else {
            (rec.h as i32, 0)
        };
        blit(
            0,
            0,
            rec.w as i32,
            rec.h as i32,
            0,
            dst_y0,
            rec.w as i32,
            dst_y1,
            GL_COLOR_BUFFER_BIT,
            GL_NEAREST as u32,
        );
        if scissor == Some(true) {
            if let Some(enable) = host.gl_enable {
                enable(GL_SCISSOR_TEST);
            }
        }
    }
    if !async_requested() {
        if let Some(finish) = host.gl_finish {
            finish();
        }
    }
    bind(GL_FRAMEBUFFER, read_fbo);
}

/// Present the native gralloc allocation through wl_shm. Unlike PresentDmabuf
/// this needs no compositor dma-buf global; wl_shm accepts any mmap-able fd.
pub(super) unsafe fn present_gralloc(surface: EGLSurface) -> Result<(), String> {
    let host = host()?;
    let rec = &mut *(surface as *mut SurfaceRec);
    let current = host
        .get_current_context
        .map(|f| f() as usize)
        .filter(|c| *c != 0)
        .unwrap_or_else(|| CURRENT_CONTEXT.with(|c| c.get()));
    let info = context_info(current);
    let target = rec
        .targets
        .iter()
        .rev()
        .find(|target| target.group == info.group)
        .and_then(|target| {
            // With the daemon-side EGL presenter (AArch64) the surface
            // allocation is sampled directly, so no auxiliary guest flip
            // target is involved. The manual android_wlegl buffer (ARM32)
            // is shown as is by Lipstick and needs the flipped copy.
            if wlegl_requested() && !wlegl_buffer_requested() {
                target.gralloc.as_ref()
            } else {
                target
                    .flip
                    .as_ref()
                    .map(|flip| &flip.buffers[flip.last].target)
                    .or(target.gralloc.as_ref())
            }
        })
        .ok_or_else(|| "surface has no gralloc target for the current context".to_string())?;
    let fd = libc::dup(target.fd);
    if fd < 0 {
        return Err(format!(
            "cannot duplicate gralloc fd: {}",
            std::io::Error::last_os_error()
        ));
    }
    let stride_bytes = target.stride * 4;
    let wlegl = target.wlegl.clone();
    if wlegl_requested() {
        sync_gralloc_for_present(host, target, rec.w, rec.h);
    }
    let result = present_gralloc_fd(fd, rec, stride_bytes, wlegl.as_ref());
    libc::close(fd);
    result
}

/// Invalidate the compositor's view of the native buffer after a GPU write.
///
/// Lipstick imports the gralloc buffer as an EGLImage once; without a
/// `hybris_gralloc_lock`/`unlock` pair it keeps showing the pixels that were
/// present at import time, so a GPU-rendered guest appears frozen while its
/// FBO content keeps changing. The CPU paint path did this implicitly, which
/// is why the probe worked and the guest did not.
unsafe fn sync_gralloc_for_present(
    host: &Host,
    target: &GrallocTarget,
    width: u32,
    height: u32,
) {
    let Ok(api) = gralloc_api(host) else {
        return;
    };
    let mut vaddr: *mut c_void = std::ptr::null_mut();
    let rc = (api.lock)(
        target.handle,
        0x303,
        0,
        0,
        width as i32,
        height as i32,
        &mut vaddr,
    );
    if rc == 0 {
        // Cache invalidation only. With WLEGL the daemon samples the native
        // texture directly, so no pixel write is needed here.
        let _ = (api.unlock)(target.handle);
    }
}

/// Send one already-synchronized native buffer. The caller retains ownership
/// of `fd`; the socket duplicates it into the daemon during SCM_RIGHTS send.
fn present_gralloc_fd(
    fd: RawFd,
    rec: &mut SurfaceRec,
    stride: u32,
    wlegl: Option<&WleglHandle>,
) -> Result<(), String> {
    let mut st = lock_state();
    let surface_id = ensure_surface_id(&mut st, rec)?;
    let mut conn = st
        .conn
        .take()
        .ok_or_else(|| "daemon is unavailable".to_string())?;
    let native_window = if env_truthy("PENGUDROID_ANDROID_LAYERS") {
        rec.native_window as u64
    } else {
        0
    };
    let result = transmit_gralloc(
        &mut conn,
        surface_id,
        rec.w,
        rec.h,
        stride,
        fd,
        native_window,
        wlegl,
    );
    if result.is_err() {
        st.conn = None;
        st.connect_attempted = false;
    } else {
        st.conn = Some(conn);
    }
    result
}

/// Worker-side variant which does not retain a raw SurfaceRec pointer.
fn present_gralloc_metadata(
    surface_id: u32,
    width: u32,
    height: u32,
    stride: u32,
    fd: RawFd,
    native_window: u64,
    wlegl: Option<&WleglHandle>,
) -> Result<(), String> {
    let mut st = lock_state();
    let mut conn = st
        .conn
        .take()
        .ok_or_else(|| "daemon is unavailable".to_string())?;
    let result = transmit_gralloc(
        &mut conn,
        surface_id,
        width,
        height,
        stride,
        fd,
        native_window,
        wlegl,
    );
    if result.is_err() {
        st.conn = None;
        st.connect_attempted = false;
    } else {
        st.conn = Some(conn);
    }
    result
}

fn transmit_gralloc(
    conn: &mut Connection,
    surface_id: u32,
    width: u32,
    height: u32,
    stride: u32,
    fd: RawFd,
    native_window: u64,
    wlegl: Option<&WleglHandle>,
) -> Result<(), String> {
    let use_wlegl = wlegl_requested();
    static REPORTED: std::sync::Once = std::sync::Once::new();
    REPORTED.call_once(|| {
        eprintln!(
            "[gleshim:gralloc] presentation transport: {}",
            if use_wlegl && wlegl.is_some() { "android_wlegl" } else { "wl_shm" }
        );
    });
    match wlegl.filter(|_| use_wlegl) {
        Some(handle) => {
            // android_wlegl needs every fd of the native handle plus its ints;
            // the daemon builds `create_handle`/`add_fd`/`create_buffer` from
            // this instead of importing the fd into a wl_shm pool.
            let message = Message::PresentShm {
                surface_id,
                width,
                height,
                stride,
                format: HAL_BGRA_8888 as u32 | NATIVE_FD_FLAG | WLEGL_FD_FLAG,
                native_window: WLEGL_MAGIC | handle.ints.len() as u64,
                crop: BufferCrop::full(width as i32, height as i32),
            };
            conn.send_message(&message, &handle.fds)
                .map_err(|e| format!("android_wlegl native handle send failed: {e}"))?;
            // The int words ride in a small extra frame right after the fd
            // list, so no protocol struct changes and the receiver can tell
            // the two parts apart by WLEGL_MAGIC.
            let mut ints = Vec::with_capacity(handle.ints.len() * 4);
            for word in &handle.ints {
                ints.extend_from_slice(&word.to_le_bytes());
            }
            conn.send_frame(&ints, &[])
                .map_err(|e| format!("android_wlegl native ints send failed: {e}"))?;
        }
        None => {
            let message = Message::PresentShm {
                surface_id,
                width,
                height,
                stride,
                format: ANDROID_RGBA_8888 | NATIVE_FD_FLAG,
                native_window,
                crop: BufferCrop::full(width as i32, height as i32),
            };
            conn.send_message(&message, &[fd])
                .map_err(|e| format!("gralloc wl_shm send failed: {e}"))?;
        }
    }
    let (reply, fds) = conn
        .recv_message()
        .map_err(|e| format!("gralloc present reply failed: {e}"))?;
    close_fds(&fds);
    match reply {
        Message::Ack => Ok(()),
        Message::Error { code, message } => {
            Err(format!("gralloc present rejected ({code}): {message}"))
        }
        other => Err(format!("unexpected gralloc present reply: {other:?}")),
    }
}

struct PendingGrallocFrame {
    surface: usize,
    surface_id: u32,
    width: u32,
    height: u32,
    stride: u32,
    fd: RawFd,
    native_window: u64,
    wlegl: Option<WleglHandle>,
    display: usize,
    sync: usize,
    frame: u64,
    touch_seq: u64,
    touch_kind: &'static str,
    touch_recv_ms: u64,
}

#[derive(Default)]
struct AsyncOutstanding {
    counts: std::collections::HashMap<usize, usize>,
}

fn async_outstanding() -> &'static (Mutex<AsyncOutstanding>, Condvar) {
    static OUTSTANDING: OnceLock<(Mutex<AsyncOutstanding>, Condvar)> = OnceLock::new();
    OUTSTANDING.get_or_init(|| (Mutex::new(AsyncOutstanding::default()), Condvar::new()))
}

fn async_presenter() -> &'static std::sync::mpsc::Sender<PendingGrallocFrame> {
    static SENDER: OnceLock<std::sync::mpsc::Sender<PendingGrallocFrame>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<PendingGrallocFrame>();
        std::thread::Builder::new()
            .name("pengudroid-present".into())
            .spawn(move || {
                while let Ok(frame) = rx.recv() {
                    let Ok(host) = host() else { break };
                    let wait_started = std::time::Instant::now();
                    let waited = host.client_wait_sync.map(|wait| unsafe { wait(
                        frame.display as EGLDisplay,
                        frame.sync as EGLSync,
                        0,
                        1_000_000_000,
                    ) });
                    let wait_elapsed = wait_started.elapsed();
                    if let Some(destroy) = host.destroy_sync {
                        unsafe { destroy(frame.display as EGLDisplay, frame.sync as EGLSync); }
                    }
                    let present_started = std::time::Instant::now();
                    let result = if waited == Some(EGL_CONDITION_SATISFIED_KHR) {
                        present_gralloc_metadata(
                            frame.surface_id, frame.width, frame.height, frame.stride, frame.fd,
                            frame.native_window,
                            frame.wlegl.as_ref(),
                        )
                    } else {
                        Err(format!("async GPU fence did not signal: {waited:?}"))
                    };
                    let present_elapsed = present_started.elapsed();
                    unsafe { libc::close(frame.fd); }
                    if result.is_ok() {
                        notify_frame_presented_vsync();
                    }
                    if std::env::var_os("PENGUDROID_TRACE_INPUT_TIMING").is_some() {
                        let now = input_trace_now_ms();
                        eprintln!(
                            "[gleshim:async] ts={now} frame={} success={} touch_seq={} kind={} touch_to_present_ms={} fence_wait_us={} present_us={}",
                            frame.frame, result.is_ok(), frame.touch_seq, frame.touch_kind,
                            now.saturating_sub(frame.touch_recv_ms),
                            wait_elapsed.as_micros(), present_elapsed.as_micros(),
                        );
                    }
                    if let Err(error) = result {
                        eprintln!("[gleshim:gralloc] async present failed: {error}");
                    }
                    let (lock, ready) = async_outstanding();
                    let mut outstanding = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(count) = outstanding.counts.get_mut(&frame.surface) {
                        *count = count.saturating_sub(1);
                        if *count == 0 { outstanding.counts.remove(&frame.surface); }
                    }
                    ready.notify_all();
                }
            })
            .expect("spawn async gralloc presenter");
        tx
    })
}

/// Wait before reusing an output buffer. This must run before the Y-flip, not
/// after it: with two outstanding frames the third flip would otherwise
/// overwrite the oldest buffer before the presenter had fenced it off.
pub(super) fn wait_gralloc_async_slot(surface: EGLSurface) {
    let (lock, ready) = async_outstanding();
    let mut outstanding = lock.lock().unwrap_or_else(|e| e.into_inner());
    while outstanding
        .counts
        .get(&(surface as usize))
        .copied()
        .unwrap_or(0)
        >= 2
    {
        outstanding = ready.wait(outstanding).unwrap_or_else(|e| e.into_inner());
    }
}

/// Queue a flipped buffer behind a real host EGL fence. No SurfaceRec pointer
/// crosses to the worker: surface destruction cannot leave it with a raw
/// reference, and all connection access remains serialized by State's mutex.
pub(super) unsafe fn queue_gralloc_async(surface: EGLSurface, host: &Host) -> Result<(), String> {
    let create = host.create_sync.ok_or("eglCreateSync is unavailable")?;
    if host.client_wait_sync.is_none() || host.destroy_sync.is_none() {
        return Err("EGL fence wait/destroy is unavailable".into());
    }
    let rec = &mut *(surface as *mut SurfaceRec);
    let current = host
        .get_current_context
        .map(|f| f() as usize)
        .filter(|c| *c != 0)
        .unwrap_or_else(|| CURRENT_CONTEXT.with(|c| c.get()));
    let info = context_info(current);
    let target = rec
        .targets
        .iter()
        .rev()
        .find(|target| target.group == info.group)
        .and_then(|target| target.flip.as_ref())
        .ok_or("surface has no gralloc flip target for the current context")?;
    if target.buffers.len() < 2 {
        return Err("async double buffering needs two output buffers".into());
    }
    let output = &target.buffers[target.last].target;
    let fd = libc::dup(output.fd);
    if fd < 0 {
        return Err(format!(
            "cannot duplicate async gralloc fd: {}",
            std::io::Error::last_os_error()
        ));
    }
    let (width, height, stride) = (rec.w, rec.h, output.stride * 4);
    // The async worker owns its own copy of the native handle description; the
    // fds themselves are duplicated by SCM_RIGHTS on every present.
    let wlegl = output.wlegl.clone();
    let display = lock_state().display;
    let sync = create(display as EGLDisplay, 0x30F9, std::ptr::null()); // EGL_SYNC_FENCE_KHR
    if sync.is_null() {
        libc::close(fd);
        return Err(format!("async eglCreateSync failed: {}", host_error(host)));
    }
    if let Some(flush) = host.gl_flush {
        flush();
    }
    let mut st = lock_state();
    let surface_id = match ensure_surface_id(&mut st, rec) {
        Ok(id) => id,
        Err(error) => {
            drop(st);
            if let Some(destroy) = host.destroy_sync {
                destroy(display as EGLDisplay, sync);
            }
            libc::close(fd);
            return Err(error);
        }
    };
    drop(st);
    let (frame, touch_seq, touch_kind, touch_recv_ms) = {
        let state = FRAME_TRACE_STATE.lock().unwrap_or_else(|e| e.into_inner());
        (
            state.frame,
            state.touch_seq,
            state.touch_kind,
            state.touch_recv_ms,
        )
    };
    let pending = PendingGrallocFrame {
        surface: surface as usize,
        surface_id,
        width,
        height,
        stride,
        fd,
        native_window: if env_truthy("PENGUDROID_ANDROID_LAYERS") {
            rec.native_window as u64
        } else {
            0
        },
        wlegl,
        display,
        sync: sync as usize,
        frame,
        touch_seq,
        touch_kind,
        touch_recv_ms,
    };
    let (lock, ready) = async_outstanding();
    let mut outstanding = lock.lock().unwrap_or_else(|e| e.into_inner());
    while outstanding
        .counts
        .get(&(surface as usize))
        .copied()
        .unwrap_or(0)
        >= 2
    {
        outstanding = ready.wait(outstanding).unwrap_or_else(|e| e.into_inner());
    }
    *outstanding.counts.entry(surface as usize).or_insert(0) += 1;
    drop(outstanding);
    if let Err(error) = async_presenter().send(pending) {
        let pending = error.0;
        if let Some(destroy) = host.destroy_sync {
            destroy(pending.display as EGLDisplay, pending.sync as EGLSync);
        }
        libc::close(pending.fd);
        let mut outstanding = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = outstanding.counts.get_mut(&(surface as usize)) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                outstanding.counts.remove(&(surface as usize));
            }
        }
        ready.notify_all();
        return Err("async presenter queue is disconnected".into());
    }
    Ok(())
}

#[derive(Default)]
struct GrallocProfile {
    count: u32,
    since: Option<std::time::Instant>,
    finish: std::time::Duration,
    flip: std::time::Duration,
    present: std::time::Duration,
    pace: std::time::Duration,
    swap: std::time::Duration,
}

pub(super) fn profile_gralloc(
    finish: std::time::Duration,
    flip: std::time::Duration,
    present: std::time::Duration,
    pace: std::time::Duration,
    swap: std::time::Duration,
) {
    static PROFILE: Mutex<GrallocProfile> = Mutex::new(GrallocProfile {
        count: 0,
        since: None,
        finish: std::time::Duration::ZERO,
        flip: std::time::Duration::ZERO,
        present: std::time::Duration::ZERO,
        pace: std::time::Duration::ZERO,
        swap: std::time::Duration::ZERO,
    });
    let mut p = PROFILE.lock().unwrap_or_else(|e| e.into_inner());
    p.count += 1;
    p.finish += finish;
    p.flip += flip;
    p.present += present;
    p.pace += pace;
    p.swap += swap;
    let elapsed = p.since.map(|since| since.elapsed()).unwrap_or_default();
    if p.since.is_none() || elapsed >= std::time::Duration::from_secs(1) {
        let n = p.count.max(1) as f64;
        let fps = if elapsed.is_zero() {
            0.0
        } else {
            p.count as f64 / elapsed.as_secs_f64()
        };
        let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1000.0 / n;
        eprintln!(
            "[gleshim:gralloc:profile] fps={fps:.1} finish={:.2}ms flip={:.2}ms present={:.2}ms pace={:.2}ms swap={:.2}ms",
            ms(p.finish),
            ms(p.flip),
            ms(p.present),
            ms(p.pace),
            ms(p.swap),
        );
        p.count = 0;
        p.finish = std::time::Duration::ZERO;
        p.flip = std::time::Duration::ZERO;
        p.present = std::time::Duration::ZERO;
        p.pace = std::time::Duration::ZERO;
        p.swap = std::time::Duration::ZERO;
        p.since = Some(std::time::Instant::now());
    }
}

#[repr(C)]
struct ProbeNativeBase {
    magic: i32,
    version: i32,
    reserved: [*mut c_void; 4],
    inc_ref: *mut c_void,
    dec_ref: *mut c_void,
}

extern "C" fn probe_noop_ref(_: *mut ProbeNativeBase) {}

#[repr(C)]
struct ProbeNativeWindowBuffer {
    common: ProbeNativeBase,
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    usage_deprecated: i32,
    layer_count: usize,
    reserved: *mut c_void,
    handle: *const c_void,
    usage: u64,
    reserved_proc: [*mut c_void; 7],
}

type GrallocInitialize = unsafe extern "C" fn(c_int);
type GrallocAllocate =
    unsafe extern "C" fn(u32, u32, c_int, u32, *mut *mut c_void, *mut u32) -> c_int;
type GrallocRelease = unsafe extern "C" fn(*mut c_void, c_int);
type GrallocDeinitialize = unsafe extern "C" fn();
type GrallocLock =
    unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32, i32, *mut *mut c_void) -> i32;
type GrallocUnlock = unsafe extern "C" fn(*mut c_void) -> i32;

struct GrallocApi {
    initialize: GrallocInitialize,
    allocate: GrallocAllocate,
    release: GrallocRelease,
    deinitialize: GrallocDeinitialize,
    lock: GrallocLock,
    unlock: GrallocUnlock,
}

/// Check the actually loaded GL stack and the complete native-buffer API.
/// AArch64 alone says nothing about whether this path is available.
pub(super) fn support(host: &Host) -> Result<(), String> {
    static SUPPORT: OnceLock<Result<(), String>> = OnceLock::new();
    SUPPORT
        .get_or_init(|| {
            if !host.gl_isolated || !hybris_host() {
                return Err("active host GL is not Hybris".into());
            }
            if host.create_image_khr.is_none()
                || host.destroy_image_khr.is_none()
                || host.gl_egl_image_target_texture_2d.is_none()
            {
                return Err("Hybris EGL native-buffer image/texture API is incomplete".into());
            }
            // Loading the API also verifies all four libgralloc entry points.
            // No allocation is attempted until a real surface requests it.
            unsafe { gralloc_api(host) }.map(|_| ())
        })
        .clone()
}

/// True only when this surface actually received a gralloc allocation. A
/// requested but failed allocation must continue through dma-buf instead.
pub(super) fn has_target(surface: EGLSurface, host: &Host) -> bool {
    if surface.is_null() {
        return false;
    }
    let rec = unsafe { &*(surface as *const SurfaceRec) };
    let current = host
        .get_current_context
        .map(|get| unsafe { get() } as usize)
        .filter(|context| *context != 0)
        .unwrap_or_else(|| CURRENT_CONTEXT.with(|current| current.get()));
    let info = context_info(current);
    rec.targets
        .iter()
        .any(|target| target.group == info.group && target.gralloc.is_some())
}

unsafe fn open_host_gralloc(host: &Host) -> *mut c_void {
    let path: &[u8] = if cfg!(target_arch = "arm") {
        b"/usr/lib/libgralloc.so.1\0"
    } else {
        b"/lib64/libgralloc.so.1\0"
    };
    let name = path.as_ptr() as *const c_char;
    let mut handle = if host.gl_isolated {
        libc::dlmopen(
            host.gl_namespace,
            name,
            libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_NOLOAD,
        )
    } else {
        libc::dlopen(name, libc::RTLD_NOW | libc::RTLD_GLOBAL | libc::RTLD_NOLOAD)
    };
    if handle.is_null() {
        handle = if host.gl_isolated {
            libc::dlmopen(host.gl_namespace, name, libc::RTLD_NOW | libc::RTLD_LOCAL)
        } else {
            libc::dlopen(name, libc::RTLD_NOW | libc::RTLD_GLOBAL)
        };
    }
    handle
}

#[cfg(target_arch = "aarch64")]
unsafe fn log_hybris_ipc_state() {
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_none() {
        return;
    }
    let Some(base) = mapped_elf_base("/usr/libexec/droid-hybris/system/lib64/libhidlbase.so")
    else {
        return;
    };
    if std::ptr::read_unaligned((base + 0x88af0) as *const u64) != 0xa9014ff4a9be7bfd {
        return;
    }
    type IpcSelf = unsafe extern "C" fn() -> *mut c_void;
    let self_fn: IpcSelf = std::mem::transmute(base + 0x88af0);
    let state = self_fn();
    if state.is_null() {
        return;
    }
    let restriction = *((state as *mut u8).add(512) as *const i32);
    let key = *((base + 0xa5c9c) as *const u32);
    let guest = mapped_elf_base("/data/lib/libbinder.so").and_then(|guest_base| {
        if std::ptr::read_unaligned((guest_base + 0x54458) as *const u64) != 0xa9014ff4a9be7bfd {
            return None;
        }
        let self_fn: IpcSelf = std::mem::transmute(guest_base + 0x54458);
        Some(self_fn())
    });
    let process = *(state as *mut *mut u8);
    let process_restriction = if process.is_null() {
        -1
    } else {
        *(process.add(192) as *const i32)
    };
    let art = mapped_elf_base("/data/lib/libart.so").and_then(|base| {
        if std::ptr::read_unaligned((base + 0x96d480) as *const u64) != 0x90001608d503245f {
            return None;
        }
        let current: IpcSelf = std::mem::transmute(base + 0x96d480);
        Some(current())
    });
    eprintln!(
        "[gleshim] Hybris HIDL state={state:p} key={key} restriction={restriction} process={process:p} process_restriction={process_restriction} guest={:p} art={:p}",
        guest.unwrap_or(std::ptr::null_mut()),
        art.unwrap_or(std::ptr::null_mut())
    );
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn log_hybris_ipc_state() {}

#[repr(C)]
struct BridgeStats {
    key_creates: usize,
    key_deletes: usize,
    getspecific: usize,
    setspecific: usize,
    once: usize,
    last_key: usize,
}

fn trace_pthread_bridge(host: &Host, phase: &str) {
    if std::env::var_os("PENGUDROID_HYBRIS_TLS_BRIDGE").is_none() || !host.gl_isolated {
        return;
    }
    let Some(gleshim) = std::env::var_os("PENGUDROID_GLESHIM") else {
        return;
    };
    let path = std::path::PathBuf::from(gleshim)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("libpengudroid_hybris_pthread.so");
    let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return;
    };
    let handle = unsafe {
        libc::dlmopen(
            host.gl_namespace,
            c_path.as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_NOLOAD,
        )
    };
    if handle.is_null() {
        eprintln!("[gleshim:bridge] {phase}: bridge is not mapped in the Hybris namespace");
        return;
    }
    type StatsFn = unsafe extern "C" fn(*mut BridgeStats);
    let Some(stats_fn) =
        sym_ptr(handle, "__pengudroid_hybris_pthread_stats").and_then(cast_fn::<StatsFn>)
    else {
        eprintln!("[gleshim:bridge] {phase}: stats symbol is unavailable");
        return;
    };
    let mut stats = BridgeStats {
        key_creates: 0,
        key_deletes: 0,
        getspecific: 0,
        setspecific: 0,
        once: 0,
        last_key: usize::MAX,
    };
    unsafe { stats_fn(&mut stats) };
    eprintln!(
        "[gleshim:bridge] {phase}: create={} delete={} get={} set={} once={} last_key={:#x}",
        stats.key_creates,
        stats.key_deletes,
        stats.getspecific,
        stats.setspecific,
        stats.once,
        stats.last_key,
    );
}

fn prepare_hybris_tls(host: &Host) {
    if std::env::var_os("PENGUDROID_HYBRIS_TLS_BRIDGE").is_some() {
        // Opt-in experiment: the bridge must prove that it serves HIDL instead
        // of falling back to the verified runtime gTLS rewrite.
        trace_pthread_bridge(host, "before HIDL allocation");
    } else {
        hybris_tls::rekey_gralloc();
    }
}

unsafe fn gralloc_api(host: &Host) -> Result<GrallocApi, String> {
    // Only Linux+Hybris/gralloc enters this architecture-specific workaround.
    // Ordinary Linux and the dma-buf path never call it.
    prepare_hybris_tls(host);
    let handle = open_host_gralloc(host);
    if handle.is_null() {
        return Err("cannot load host libgralloc.so.1".into());
    }
    Ok(GrallocApi {
        initialize: sym_ptr(handle, "hybris_gralloc_initialize")
            .and_then(cast_fn)
            .ok_or("hybris_gralloc_initialize is unavailable")?,
        allocate: sym_ptr(handle, "hybris_gralloc_allocate")
            .and_then(cast_fn)
            .ok_or("hybris_gralloc_allocate is unavailable")?,
        release: sym_ptr(handle, "hybris_gralloc_release")
            .and_then(cast_fn)
            .ok_or("hybris_gralloc_release is unavailable")?,
        deinitialize: sym_ptr(handle, "hybris_gralloc_deinitialize")
            .and_then(cast_fn)
            .ok_or("hybris_gralloc_deinitialize is unavailable")?,
        lock: sym_ptr(handle, "hybris_gralloc_lock")
            .and_then(cast_fn)
            .ok_or("hybris_gralloc_lock is unavailable")?,
        unlock: sym_ptr(handle, "hybris_gralloc_unlock")
            .and_then(cast_fn)
            .ok_or("hybris_gralloc_unlock is unavailable")?,
    })
}

unsafe fn allocate_gralloc(host: &Host, width: u32, height: u32) -> Result<GrallocTarget, String> {
    let api = gralloc_api(host)?;
    let mut handle = std::ptr::null_mut();
    let mut stride = 0u32;
    (api.initialize)(0);
    trace_pthread_bridge(host, "after gralloc initialize");
    log_hybris_ipc_state();
    // HW_RENDER | HW_TEXTURE | SW_READ_OFTEN: request linear CPU-readable data.
    let rc = (api.allocate)(width, height, 5, 0x303, &mut handle, &mut stride);
    trace_pthread_bridge(host, "after gralloc allocate");
    if rc != 0 || handle.is_null() {
        (api.deinitialize)();
        return Err(format!(
            "hybris_gralloc_allocate failed: rc={rc} handle={handle:p}"
        ));
    }
    let header = handle as *const i32;
    let num_fds = *header.add(1);
    if num_fds < 1 {
        (api.release)(handle, 1);
        (api.deinitialize)();
        return Err("gralloc handle contains no file descriptor".into());
    }
    let mut fd = *header.add(3);
    if std::env::var_os("PENGUDROID_GRALLOC_SHM").is_some() {
        let length = stride as usize * height as usize * 4;
        let mut selected = None;
        let mut mmapable = Vec::new();
        for index in 0..num_fds {
            let candidate = *header.add(3 + index as usize);
            if candidate < 0 {
                continue;
            }
            let mapping = libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                candidate,
                0,
            );
            let ok = mapping != libc::MAP_FAILED;
            if ok {
                libc::munmap(mapping, length);
                mmapable.push(index);
                if selected.is_none() {
                    selected = Some(index);
                }
            }
            eprintln!("[gleshim:gralloc] fd index={index} fd={candidate} mmap={ok}");
        }
        if let Some(requested) = std::env::var("PENGUDROID_GRALLOC_FD_INDEX")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
        {
            if mmapable.contains(&requested) {
                selected = Some(requested);
            } else {
                selected = None;
            }
        }
        let Some(index) = selected else {
            (api.release)(handle, 1);
            (api.deinitialize)();
            return Err("no gralloc fd is mmap-able for wl_shm".into());
        };
        fd = *header.add(3 + index as usize);
        eprintln!("[gleshim:gralloc] selected mmap-able fd index {index} for wl_shm");
    }
    if fd < 0 {
        (api.release)(handle, 1);
        (api.deinitialize)();
        return Err("gralloc handle has an invalid file descriptor".into());
    }
    Ok(GrallocTarget { handle, fd, stride, wlegl: wlegl_handle(handle) })
}

fn native_window_buffer(
    target: &GrallocTarget,
    width: u32,
    height: u32,
) -> ProbeNativeWindowBuffer {
    ProbeNativeWindowBuffer {
        common: ProbeNativeBase {
            magic: i32::from_be_bytes(*b"_bfr"),
            version: std::mem::size_of::<ProbeNativeWindowBuffer>() as i32,
            reserved: [std::ptr::null_mut(); 4],
            inc_ref: probe_noop_ref as *mut () as *mut c_void,
            dec_ref: probe_noop_ref as *mut () as *mut c_void,
        },
        width: width as i32,
        height: height as i32,
        stride: target.stride as i32,
        format: 5, // HAL_PIXEL_FORMAT_BGRA_8888
        usage_deprecated: 0x303,
        layer_count: 1,
        reserved: std::ptr::null_mut(),
        handle: target.handle as *const c_void,
        usage: 0x303,
        reserved_proc: [std::ptr::null_mut(); 7],
    }
}

/// Wrap a gralloc allocation as the texture used by the surface FBO.
pub(super) unsafe fn attach_gralloc_texture(
    host: &Host,
    texture: u32,
    width: u32,
    height: u32,
) -> Result<GrallocTarget, String> {
    let started = std::time::Instant::now();
    let target = allocate_gralloc(host, width, height)?;
    let mut native = native_window_buffer(&target, width, height);
    let image_target = host
        .gl_egl_image_target_texture_2d
        .ok_or("glEGLImageTargetTexture2DOES is unavailable")?;
    let image = (host
        .create_image_khr
        .ok_or("eglCreateImageKHR unavailable")?)(
        lock_state().display as EGLDisplay,
        std::ptr::null_mut(),
        0x3140, // EGL_NATIVE_BUFFER_ANDROID
        &mut native as *mut ProbeNativeWindowBuffer as EGLClientBuffer,
        std::ptr::null(),
    );
    if image.is_null() {
        return Err(format!(
            "gralloc EGLImage creation failed: {}",
            host_error(host)
        ));
    }
    image_target(GL_TEXTURE_2D, image);
    let error = host.gl_get_error.map(|get| get()).unwrap_or(0);
    if let Some(destroy) = host.destroy_image_khr {
        destroy(lock_state().display as EGLDisplay, image);
    }
    if error != 0 {
        return Err(format!(
            "glEGLImageTargetTexture2DOES failed: GL error {error:#x}"
        ));
    }
    if std::env::var_os("PENGUDROID_PROFILE_GRALLOC").is_some() {
        eprintln!(
            "[gleshim:gralloc:setup] allocation+EGLImage+texture={:.2}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(target)
}

/// Allocate a native output buffer and its FBO for the GPU Y-flip blit.
pub(super) unsafe fn create_gralloc_flip_buffer(
    host: &Host,
    width: u32,
    height: u32,
) -> Result<GrallocFlipBuffer, String> {
    let started = std::time::Instant::now();
    let gen_texture = host.gl_gen_textures.ok_or("glGenTextures unavailable")?;
    let bind_texture = host.gl_bind_texture.ok_or("glBindTexture unavailable")?;
    let target_image = host
        .gl_egl_image_target_texture_2d
        .ok_or("EGL image texture unavailable")?;
    let _ = target_image;
    let mut texture = 0u32;
    gen_texture(1, &mut texture);
    if texture == 0 {
        return Err("flip glGenTextures returned 0".into());
    }
    bind_texture(GL_TEXTURE_2D, texture);
    let target = attach_gralloc_texture(host, texture, width, height)?;
    if let Some(set) = host.gl_tex_parameter_i {
        set(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        set(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    }
    let gen_fbo = host
        .gl_gen_framebuffers
        .ok_or("glGenFramebuffers unavailable")?;
    let bind_fbo = host
        .gl_bind_framebuffer
        .ok_or("glBindFramebuffer unavailable")?;
    let attach = host
        .gl_framebuffer_texture_2d
        .ok_or("glFramebufferTexture2D unavailable")?;
    let mut fbo = 0u32;
    gen_fbo(1, &mut fbo);
    if fbo == 0 {
        return Err("flip glGenFramebuffers returned 0".into());
    }
    bind_fbo(GL_FRAMEBUFFER, fbo);
    attach(
        GL_FRAMEBUFFER,
        GL_COLOR_ATTACHMENT0,
        GL_TEXTURE_2D,
        texture,
        0,
    );
    if let Some(check) = host.gl_check_framebuffer_status {
        if check(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
            return Err("flip framebuffer is incomplete".into());
        }
    }
    if std::env::var_os("PENGUDROID_PROFILE_GRALLOC").is_some() {
        eprintln!(
            "[gleshim:gralloc:setup] flip buffer+FBO={:.2}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(GrallocFlipBuffer { target, fbo })
}

pub(super) unsafe fn create_gralloc_flip(
    host: &Host,
    width: u32,
    height: u32,
) -> Result<GrallocFlip, String> {
    Ok(GrallocFlip {
        buffers: vec![create_gralloc_flip_buffer(host, width, height)?],
        next: 0,
        last: 0,
    })
}

/// Opt-in host gralloc probe. It does not replace the normal present path:
/// after logging allocation/EGLImage support, eglSwapBuffers continues through
/// the selected dma-buf or SHM fallback.
pub(super) fn probe_gralloc_once() {
    if std::env::var_os("PENGUDROID_PROBE_GRALLOC").is_none() {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Err(error) = unsafe { probe_gralloc() } {
            eprintln!("[gleshim:gralloc] {error}");
        }
    });
}

unsafe fn probe_gralloc() -> Result<(), String> {
    let host = host()?;
    support(host)?;
    let gralloc = open_host_gralloc(host);
    let gralloc = if gralloc.is_null() {
        None
    } else {
        Some(gralloc)
    }
    .ok_or_else(|| {
        let error = unsafe { libc::dlerror() };
        if error.is_null() {
            "cannot load host libgralloc.so.1".to_string()
        } else {
            format!(
                "cannot load host libgralloc.so.1: {}",
                unsafe { std::ffi::CStr::from_ptr(error) }.to_string_lossy(),
            )
        }
    })?;
    type Initialize = unsafe extern "C" fn(c_int);
    type Allocate = unsafe extern "C" fn(u32, u32, c_int, u32, *mut *mut c_void, *mut u32) -> c_int;
    type Release = unsafe extern "C" fn(*mut c_void, c_int);
    type Deinitialize = unsafe extern "C" fn();
    let initialize: Initialize = sym_ptr(gralloc, "hybris_gralloc_initialize")
        .and_then(cast_fn)
        .ok_or("hybris_gralloc_initialize is unavailable")?;
    let allocate: Allocate = sym_ptr(gralloc, "hybris_gralloc_allocate")
        .and_then(cast_fn)
        .ok_or("hybris_gralloc_allocate is unavailable")?;
    let release: Release = sym_ptr(gralloc, "hybris_gralloc_release")
        .and_then(cast_fn)
        .ok_or("hybris_gralloc_release is unavailable")?;
    let deinitialize: Deinitialize = sym_ptr(gralloc, "hybris_gralloc_deinitialize")
        .and_then(cast_fn)
        .ok_or("hybris_gralloc_deinitialize is unavailable")?;
    prepare_hybris_tls(host);
    let mut handle = std::ptr::null_mut();
    let mut stride = 0u32;
    initialize(0);
    // HW_RENDER | HW_TEXTURE | SW_READ_OFTEN.
    let rc = allocate(64, 64, 5, 0x303, &mut handle, &mut stride);
    if rc != 0 || handle.is_null() {
        deinitialize();
        return Err(format!(
            "hybris_gralloc_allocate failed: rc={rc} handle={handle:p}"
        ));
    }
    eprintln!(
        "[gleshim:gralloc] allocated 64x64 stride={} handle={:p}",
        stride, handle,
    );
    let header = handle as *const i32;
    let num_fds = *header.add(1);
    let first_fd = if num_fds > 0 { *header.add(3) } else { -1 };
    eprintln!(
        "[gleshim:gralloc] native handle fds={} first_fd={}",
        num_fds, first_fd,
    );
    let mut native = ProbeNativeWindowBuffer {
        common: ProbeNativeBase {
            magic: i32::from_be_bytes(*b"_bfr"),
            version: std::mem::size_of::<ProbeNativeWindowBuffer>() as i32,
            reserved: [std::ptr::null_mut(); 4],
            inc_ref: probe_noop_ref as *mut () as *mut c_void,
            dec_ref: probe_noop_ref as *mut () as *mut c_void,
        },
        width: 64,
        height: 64,
        stride: stride as i32,
        format: 5,
        usage_deprecated: 0x303,
        layer_count: 1,
        reserved: std::ptr::null_mut(),
        handle: handle as *const c_void,
        usage: 0x303,
        reserved_proc: [std::ptr::null_mut(); 7],
    };
    let create_image = host
        .create_image_khr
        .ok_or("eglCreateImageKHR unavailable")?;
    let image = create_image(
        lock_state().display as EGLDisplay,
        std::ptr::null_mut(),
        0x3140, // EGL_NATIVE_BUFFER_ANDROID
        &mut native as *mut ProbeNativeWindowBuffer as EGLClientBuffer,
        std::ptr::null(),
    );
    if image.is_null() {
        eprintln!(
            "[gleshim:gralloc] EGL_NATIVE_BUFFER_ANDROID image failed: {}",
            host_error(host),
        );
    } else {
        eprintln!("[gleshim:gralloc] EGL_NATIVE_BUFFER_ANDROID image created successfully");
        if let Some(destroy) = host.destroy_image_khr {
            destroy(lock_state().display as EGLDisplay, image);
        }
    }
    release(handle, 1);
    deinitialize();
    Ok(())
}

/// Cap the guest's swap rate.
pub(super) fn pace_gralloc_swap(surface: EGLSurface) {
    let id = unsafe { (*(surface as *const SurfaceRec)).surface_id };
    let key = if id == 0 {
        surface as usize
    } else {
        usize::MAX - id as usize
    };
    pace_swap(key);
}
