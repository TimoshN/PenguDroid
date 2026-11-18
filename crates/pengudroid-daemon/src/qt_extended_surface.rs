//! Lipstick's optional Qt extended-surface protocol.
//!
//! This is deliberately layered on top of xdg-shell or wl_shell; it does not
//! create the window role and is unrelated to EGL buffer presentation.

#[cfg(feature = "wayland")]
pub mod imp {
    use wayland_backend;
    use wayland_client;
    use wayland_client::protocol::wl_surface;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/surface-extension.xml");
    }

    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/surface-extension.xml");
}

/// QDataStream/QVariant bool, big-endian, with a non-null value flag.
pub fn qvariant_bool(value: bool) -> Vec<u8> {
    let mut out = 1u32.to_be_bytes().to_vec();
    out.push(0);
    out.push(u8::from(value));
    out
}

/// QDataStream/QVariant quint64, big-endian, with a non-null value flag.
pub fn qvariant_u64(value: u64) -> Vec<u8> {
    let mut out = 5u32.to_be_bytes().to_vec();
    out.push(0);
    out.extend_from_slice(&value.to_be_bytes());
    out
}

/// QDataStream/QVariant double, big-endian, with a non-null value flag.
pub fn qvariant_double(value: f64) -> Vec<u8> {
    let mut out = 6u32.to_be_bytes().to_vec();
    out.push(0);
    out.extend_from_slice(&value.to_bits().to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn qvariant_encoding_matches_qdatastream() {
        assert_eq!(super::qvariant_bool(true), [0, 0, 0, 1, 0, 1]);
        assert_eq!(super::qvariant_bool(false), [0, 0, 0, 1, 0, 0]);
        assert_eq!(
            super::qvariant_u64(1),
            [0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        );
        assert_eq!(
            super::qvariant_double(1.0),
            [0, 0, 0, 6, 0, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0],
        );
    }
}
