//! `wl_shm` transport used by the probe's `--shm` comparison mode.
//!
//! The native allocation is shared through an mmap-able fd. The compositor
//! reads CPU-visible pixels from a `wl_shm_pool`; no Android native handle is
//! sent through `android_wlegl` on this path.

use std::os::fd::{BorrowedFd, RawFd};

use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use crate::wlegl::State;

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

empty_dispatch!(WlShm);
empty_dispatch!(WlShmPool);

/// Bind the compositor's shared-memory global for the comparison transport.
pub fn bind(globals: &GlobalList, qh: &QueueHandle<State>) -> Result<WlShm, String> {
    globals
        .bind(qh, 1..=2, ())
        .map_err(|e| format!("wl_shm unavailable: {e}"))
}

/// Wrap one mmap-able native allocation in a `wl_shm` buffer.
///
/// `stride` is measured in pixels, as in the gralloc allocation. Wayland
/// receives it converted to bytes. The fd is duplicated because the pool
/// request passes ownership of its duplicate to the compositor while gralloc
/// retains the original descriptor.
pub fn create_buffer(
    shm: &WlShm,
    fd: RawFd,
    width: u32,
    height: u32,
    stride: u32,
    qh: &QueueHandle<State>,
) -> Result<WlBuffer, String> {
    if fd < 0 {
        return Err("the gralloc allocation has no mmap-able fd for wl_shm".into());
    }
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate < 0 {
        return Err(format!(
            "cannot duplicate the gralloc fd: {}",
            std::io::Error::last_os_error()
        ));
    }

    let size = stride as i32 * height as i32 * 4;
    // SAFETY: `duplicate` is a fresh fd owned by this scope; the pool request
    // passes it to the compositor over SCM_RIGHTS.
    let borrowed = unsafe { BorrowedFd::borrow_raw(duplicate) };
    let pool = shm.create_pool(borrowed, size, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32 * 4,
        wl_shm::Format::Xrgb8888,
        qh,
        (),
    );
    pool.destroy();
    // The compositor holds its own reference to the pool memory.
    unsafe { libc::close(duplicate) };
    Ok(buffer)
}
