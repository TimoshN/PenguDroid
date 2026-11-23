//! EGL_MESA/EXT dma-buf export and daemon presentation.
//!
//! This backend does not call Hybris gralloc or its TLS workaround.

use std::ffi::c_int;
use std::os::fd::RawFd;

use pengudroid_proto::Message;

use super::{
    CURRENT_CONTEXT, EGL_GL_TEXTURE_2D_KHR, EGL_TRUE, EGLClientBuffer, EGLContext, EGLDisplay,
    EGLImageKHR, EGLSurface, EglExportDMABUFImageMESA, EglExportDMABUFImageQueryMESA, Host,
    SurfaceRec, close_fds, ensure_fbo, ensure_host_context, ensure_surface_id, host, host_error,
    lock_state,
};

// ---------------------------------------------------------------------------
// dma-buf export
// ---------------------------------------------------------------------------

/// Whether the active host EGL has the complete image/dma-buf export API.
/// This is a runtime property of the loaded driver, not of the CPU architecture.
pub(super) fn is_supported(host: &Host) -> bool {
    host.create_image_khr.is_some()
        && host.destroy_image_khr.is_some()
        && host.export_dmabuf_query.is_some()
        && host.export_dmabuf_export.is_some()
}

/// Plane 0 of an exported EGLImage. The caller owns `fd`.
#[derive(Debug, Clone, Copy)]
pub struct DmabufExport {
    pub fd: RawFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub offset: u32,
}

/// Export the texture of `surface` as a dma-buf.
///
/// The host context must be current. The internal EGL context value is passed
/// to `eglCreateImageKHR` — Mesa's Iris driver dereferences it and segfaults if
/// `EGL_NO_CONTEXT` is used.
pub fn export_surface_dmabuf(surface: EGLSurface) -> Result<DmabufExport, String> {
    if surface.is_null() {
        return Err("export: null EGLSurface".into());
    }
    let host = host()?;

    let create_image = host
        .create_image_khr
        .ok_or_else(|| "EGL_KHR_image_base (eglCreateImageKHR) is unavailable".to_string())?;
    let destroy_image = host
        .destroy_image_khr
        .ok_or_else(|| "eglDestroyImageKHR is unavailable".to_string())?;
    unsafe {
        // Direct callers of this helper may not have made the context current
        // even though they set one up earlier.
        ensure_host_context(host)?;
    }
    let (texture, _) = unsafe { ensure_fbo(surface, host)? };
    let rec = unsafe { &*(surface as *const SurfaceRec) };
    if let Some(target) = rec
        .targets
        .iter()
        .find(|target| target.tex == texture)
        .and_then(|target| {
            target
                .flip
                .as_ref()
                .map(|flip| &flip.buffers[flip.last].target)
                .or(target.gralloc.as_ref())
        })
    {
        let fd = unsafe { libc::dup(target.fd) };
        if fd < 0 {
            return Err(format!(
                "cannot duplicate gralloc fd: {}",
                std::io::Error::last_os_error()
            ));
        }
        return Ok(DmabufExport {
            fd,
            width: rec.w,
            height: rec.h,
            stride: target.stride * 4,
            // HAL_BGRA_8888 bytes correspond to DRM RGBA8888 on AArch64 LE.
            fourcc: 0x3432_4152,
            modifier: 0, // DRM_FORMAT_MOD_LINEAR
            offset: 0,
        });
    }
    let query_image = host
        .export_dmabuf_query
        .ok_or_else(|| "EGL_MESA/EXT_image_dma_buf_export is unavailable".to_string())?;
    let export_image = host
        .export_dmabuf_export
        .ok_or_else(|| "EGL_MESA/EXT_image_dma_buf_export is unavailable".to_string())?;

    // The display and current context. Never pass `EGL_NO_CONTEXT`.
    let (cached_display, cached_context) = {
        let st = lock_state();
        let own = CURRENT_CONTEXT.with(|c| c.get());
        (st.display, if own != 0 { own } else { st.context })
    };
    let current_context = host
        .get_current_context
        .map(|f| unsafe { f() })
        .unwrap_or(std::ptr::null_mut());
    let context = if current_context.is_null() {
        if cached_context == 0 {
            return Err("export: no current EGLContext".into());
        }
        cached_context as EGLContext
    } else {
        current_context
    };
    // Prefer the display this process actually initialized: every image this
    // module creates is created against that one, and Mesa's dma-buf entry
    // points validate the display before anything else.
    let display = if cached_display != 0 {
        cached_display as EGLDisplay
    } else {
        host.get_current_display
            .map(|f| unsafe { f() })
            .unwrap_or(std::ptr::null_mut())
    };
    if display.is_null() {
        return Err("export: display is not initialized".into());
    }

    // SAFETY: the texture belongs to the current context and `image` is
    // destroyed below on every path.
    let image = unsafe {
        create_image(
            display,
            context,
            EGL_GL_TEXTURE_2D_KHR,
            texture as usize as EGLClientBuffer,
            std::ptr::null(),
        )
    };
    if image.is_null() {
        return Err(format!("eglCreateImageKHR failed: {}", host_error(host)));
    }

    let result = export_plane0(
        host,
        display,
        image,
        query_image,
        export_image,
        rec.w,
        rec.h,
    );
    // SAFETY: `image` was created above and is not used afterwards.
    unsafe { destroy_image(display, image) };
    result
}

fn export_plane0(
    host: &Host,
    display: EGLDisplay,
    image: EGLImageKHR,
    query_image: EglExportDMABUFImageQueryMESA,
    export_image: EglExportDMABUFImageMESA,
    width: u32,
    height: u32,
) -> Result<DmabufExport, String> {
    let mut fourcc: c_int = 0;
    let mut num_planes: c_int = 0;
    let mut modifiers = [0u64; 4];

    // SAFETY: `image` is live, the out-pointers are valid, and `modifiers` has
    // room for the number of planes the extension reports.
    let ok = unsafe {
        query_image(
            display,
            image,
            &mut fourcc,
            &mut num_planes,
            modifiers.as_mut_ptr(),
        )
    };
    if ok != EGL_TRUE {
        return Err(format!(
            "eglExportDMABUFImageQueryMESA failed: {}",
            host_error(host)
        ));
    }
    if num_planes < 1 {
        return Err("dmabuf export reported no planes".into());
    }
    let planes = (num_planes as usize).min(modifiers.len());

    let mut fds = [-1i32; 4];
    let mut strides = [0i32; 4];
    let mut offsets = [0i32; 4];
    // SAFETY: three arrays of at least `num_planes` `int`s, as required.
    let ok = unsafe {
        export_image(
            display,
            image,
            fds.as_mut_ptr(),
            strides.as_mut_ptr(),
            offsets.as_mut_ptr(),
        )
    };
    if ok != EGL_TRUE {
        close_exported_fds(&fds[..planes]);
        return Err(format!(
            "eglExportDMABUFImageMESA failed: {}",
            host_error(host)
        ));
    }
    if fds[0] < 0 {
        close_exported_fds(&fds[..planes]);
        return Err("dmabuf export returned an invalid fd".into());
    }

    // Only plane 0 is presented; close any additional planes (avoiding dupes).
    for i in 1..planes {
        if fds[i] >= 0 && !fds[..i].contains(&fds[i]) {
            // SAFETY: this fd was just created by the export extension.
            unsafe { libc::close(fds[i]) };
        }
    }

    Ok(DmabufExport {
        fd: fds[0],
        width,
        height,
        stride: strides[0] as u32,
        fourcc: fourcc as u32,
        modifier: modifiers[0],
        offset: offsets[0] as u32,
    })
}

fn close_exported_fds(fds: &[RawFd]) {
    for &fd in fds {
        if fd >= 0 {
            // SAFETY: these fds are owned by us and not closed elsewhere.
            unsafe { libc::close(fd) };
        }
    }
}

/// Send one `PresentDmabuf` and wait for exactly one reply.
///
/// The connection is taken out of the global state for the duration, so even a
/// blocked `recv` does not leave the mutex in a state that other EGL calls
/// would deadlock on; the mutex is held only for this single request/response.
pub(super) unsafe fn present_dmabuf(
    surface: EGLSurface,
    width: u32,
    height: u32,
    stride: u32,
    fourcc: u32,
    modifier: u64,
    offset: u32,
    fd: RawFd,
) -> Result<(), String> {
    let mut st = lock_state();
    // SAFETY: every non-null surface is a `SurfaceRec` this module allocated.
    let rec = &mut *(surface as *mut SurfaceRec);
    let surface_id = ensure_surface_id(&mut st, rec)?;

    let conn = st
        .conn
        .take()
        .ok_or_else(|| "daemon is unavailable".to_string())?;

    let mut io_failure = false;
    let result = (|| -> Result<(), String> {
        let msg = Message::PresentDmabuf {
            surface_id,
            width,
            height,
            stride,
            fourcc,
            modifier,
            offset,
            top_down: false,
        };
        conn.send_message(&msg, &[fd]).map_err(|e| {
            io_failure = true;
            format!("PresentDmabuf send failed: {e}")
        })?;
        let (reply, fds) = conn.recv_message().map_err(|e| {
            io_failure = true;
            format!("PresentDmabuf reply failed: {e}")
        })?;
        close_fds(&fds);
        match reply {
            Message::Ack => Ok(()),
            Message::Error { code, message } => {
                Err(format!("present rejected ({code}): {message}"))
            }
            other => Err(format!("unexpected present reply: {other:?}")),
        }
    })();

    if io_failure {
        // Dead socket: allow a later swap to reconnect.
        st.conn = None;
        st.connect_attempted = false;
    } else {
        st.conn = Some(conn);
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        match &result {
            Ok(()) => eprintln!("[gleshim] presented surface {surface_id} {width}x{height}"),
            Err(e) => eprintln!("[gleshim] present failed: {e}"),
        }
    }
    result
}
