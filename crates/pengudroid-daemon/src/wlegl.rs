//! Sailfish/hybris `android_wlegl` client protocol (v2), used by the daemon to
//! hand an Android native buffer to the compositor as a GPU-backed `wl_buffer`
//! instead of a CPU-read `wl_shm` pool.
//!
//! The XML is shared with `pengudroid wlegl-probe`, which already verified the
//! transport on Lipstick (Aurora 5.2.1.200). Only `create_handle`/`add_fd`/
//! `create_buffer` are needed for guest presentation; the v2
//! `get_server_buffer_handle` request is available for future compositor-owned
//! allocations.

#[cfg(feature = "wayland")]
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
