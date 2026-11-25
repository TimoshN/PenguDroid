//! Wayland toplevel construction and the daemon's dark status placeholder.
//!
//! Buffer presentation, input dispatch, and the lifetime of registered guest
//! surfaces remain in `graphics`. This module only creates the native window
//! objects and paints text into a CPU-visible RGBA buffer.

use smithay_client_toolkit::compositor::{CompositorState, SurfaceData};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_shell::WlShell;
use wayland_client::protocol::wl_shell_surface::WlShellSurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_wm_base::XdgWmBase,
};

use crate::qt_extended_surface::imp::{
    qt_extended_surface::QtExtendedSurface, qt_surface_extension::QtSurfaceExtension,
};

pub const STARTING_STATUS: &str = "STARTING ANDROID RUNTIME...";

/// Wayland objects that make up one newly created toplevel.
///
/// `graphics` places them in its pending/live surface maps and retains ownership
/// for the rest of the window's lifetime.
pub struct ToplevelParts {
    pub wl_surface: WlSurface,
    pub xdg_surface: Option<XdgSurface>,
    pub xdg_toplevel: Option<XdgToplevel>,
    pub shell_surface: Option<WlShellSurface>,
    pub decoration: Option<ZxdgToplevelDecorationV1>,
    pub extended: Option<QtExtendedSurface>,
}

/// Create an xdg-toplevel, falling back to Aurora's legacy `wl_shell` role.
///
/// `U` is the surface id carried by role/decoration events. The caller owns the
/// configure roundtrip and registers the returned objects in its surface maps.
#[allow(clippy::too_many_arguments)]
pub fn create_toplevel<D, U>(
    compositor: &WlCompositor,
    frame_compositor: Option<&CompositorState>,
    wm_base: Option<&XdgWmBase>,
    shell: Option<&WlShell>,
    decoration_manager: Option<&ZxdgDecorationManagerV1>,
    surface_extension: Option<&QtSurfaceExtension>,
    qh: &QueueHandle<D>,
    surface_id: U,
    width: u32,
    height: u32,
    title: &str,
    resizable: bool,
) -> Option<ToplevelParts>
where
    D: Dispatch<WlSurface, ()>
        + Dispatch<WlSurface, SurfaceData>
        + Dispatch<XdgSurface, ()>
        + Dispatch<XdgToplevel, U>
        + Dispatch<WlShellSurface, U>
        + Dispatch<ZxdgToplevelDecorationV1, U>
        + Dispatch<QtExtendedSurface, U>
        + 'static,
    U: Clone + Send + Sync + 'static,
{
    if wm_base.is_none() && shell.is_none() {
        return None;
    }

    let wl_surface = match frame_compositor {
        Some(compositor) => compositor.create_surface(qh),
        None => compositor.create_surface(qh, ()),
    };
    let mut xdg_surface = None;
    let mut xdg_toplevel = None;
    let mut shell_surface = None;
    let mut decoration = None;

    if let Some(wm_base) = wm_base {
        let xdg = wm_base.get_xdg_surface(&wl_surface, qh, ());
        let toplevel = xdg.get_toplevel(qh, surface_id.clone());
        toplevel.set_title(title.to_string());
        if let Some(manager) = decoration_manager {
            let object = manager.get_toplevel_decoration(&toplevel, qh, surface_id.clone());
            object.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            decoration = Some(object);
        }
        // A viewport can scale existing guest buffers during an interactive
        // resize. Without it, do not advertise unsupported resizing.
        if resizable {
            toplevel.set_min_size(160, 120);
        } else {
            toplevel.set_min_size(width as i32, height as i32);
            toplevel.set_max_size(width as i32, height as i32);
        }
        xdg_surface = Some(xdg);
        xdg_toplevel = Some(toplevel);
    } else if let Some(shell) = shell {
        let object = shell.get_shell_surface(&wl_surface, qh, surface_id.clone());
        object.set_toplevel();
        object.set_title(title.to_string());
        object.set_class("io.github.denizomer10.PenguDroid".to_string());
        shell_surface = Some(object);
    }

    // Optional Lipstick integration; wl_shell/xdg above remains the role.
    let extended = surface_extension.map(|extension| {
        let extended = extension.get_extended_surface(&wl_surface, qh, surface_id);
        extended.set_content_orientation_mask(1 | 2 | 4 | 8);
        extended.set_window_flags(0);
        extended.update_generic_property(
            "BACKGROUND_VISIBLE".to_string(),
            crate::qt_extended_surface::qvariant_bool(true),
        );
        extended.update_generic_property(
            "WINDOW_OPACITY".to_string(),
            crate::qt_extended_surface::qvariant_double(1.0),
        );
        extended.update_generic_property(
            "WINID".to_string(),
            crate::qt_extended_surface::qvariant_u64(wl_surface.id().protocol_id() as u64),
        );
        extended.update_generic_property(
            "STATUSBAR_VISIBLE".to_string(),
            crate::qt_extended_surface::qvariant_bool(true),
        );
        extended
    });

    // Select the role before the first buffer is attached. With wl_shell the
    // caller finalizes immediately; xdg-shell waits for the initial configure.
    wl_surface.commit();
    Some(ToplevelParts {
        wl_surface,
        xdg_surface,
        xdg_toplevel,
        shell_surface,
        decoration,
        extended,
    })
}

/// Paint the dark loading/error texture and centred status text.
///
/// Pixels are little-endian ARGB8888 in memory (B, G, R, A), matching the
/// `wl_shm` buffer used for the placeholder. Drawing is clipped at the buffer
/// boundaries, so long errors and very small windows are safe.
pub fn paint_status(
    bytes: &mut [u8],
    stride: u32,
    width: u32,
    height: u32,
    status: &str,
    status_color: [u8; 3],
) {
    for y in 0..height {
        for x in 0..width {
            let offset = (y * stride + x * 4) as usize;
            if offset + 3 < bytes.len() {
                bytes[offset] = 0;
                bytes[offset + 1] = 0;
                bytes[offset + 2] = 0;
                bytes[offset + 3] = 255;
            }
        }
    }

    let title = "PENGUDROID";
    let title_scale = (width / 120).max(2) as i32;
    let status_scale = (title_scale / 2).max(1);
    let center_x = width as i32 / 2;
    let center_y = height as i32 / 2;
    let title_width = title.len() as i32 * 6 * title_scale;
    let status_width = status.chars().count() as i32 * 6 * status_scale;
    draw_text(
        bytes,
        stride,
        width,
        height,
        center_x - title_width / 2,
        center_y - 40 * title_scale,
        title,
        title_scale,
        [255, 255, 255],
    );
    draw_text(
        bytes,
        stride,
        width,
        height,
        center_x - status_width / 2,
        center_y + 10 * title_scale,
        status,
        status_scale,
        status_color,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    bytes: &mut [u8],
    stride: u32,
    width: u32,
    height: u32,
    x0: i32,
    y0: i32,
    text: &str,
    scale: i32,
    rgb: [u8; 3],
) {
    for (index, character) in text.chars().enumerate() {
        let glyph = glyph(character.to_ascii_uppercase());
        let glyph_x = x0 + index as i32 * 6 * scale;
        for row in 0..7i32 {
            for column in 0..5i32 {
                if glyph[row as usize] & (1 << (4 - column)) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = glyph_x + column * scale + sx;
                        let y = y0 + row * scale + sy;
                        if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                            continue;
                        }
                        let offset = (y as u32 * stride + x as u32 * 4) as usize;
                        if offset + 3 < bytes.len() {
                            bytes[offset] = rgb[2];
                            bytes[offset + 1] = rgb[1];
                            bytes[offset + 2] = rgb[0];
                            bytes[offset + 3] = 255;
                        }
                    }
                }
            }
        }
    }
}

/// 5x7 bitmap font for status/error text on the loading screen.
fn glyph(character: char) -> [u8; 7] {
    match character {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '.' => [0, 0, 0, 0, 0, 0x0C, 0x0C],
        ':' => [0, 0x0C, 0x0C, 0, 0x0C, 0x0C, 0],
        '-' => [0, 0, 0, 0x1F, 0, 0, 0],
        '!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0, 0x04],
        '/' => [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10],
        '_' => [0, 0, 0, 0, 0, 0, 0x1F],
        _ => [0, 0, 0, 0, 0, 0, 0],
    }
}

#[cfg(test)]
mod tests {
    use super::paint_status;

    #[test]
    fn status_texture_is_opaque_and_contains_text() {
        let (width, height, stride) = (480, 800, 480 * 4);
        let mut pixels = vec![0x55; (stride * height) as usize];
        paint_status(
            &mut pixels,
            stride,
            width,
            height,
            "GUEST CONNECTED",
            [200, 200, 200],
        );

        assert!(pixels.chunks_exact(4).any(|pixel| pixel == [0, 0, 0, 255]));
        assert!(pixels
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 255, 255, 255]));
        assert!(pixels
            .chunks_exact(4)
            .any(|pixel| pixel == [200, 200, 200, 255]));
        assert!(pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn long_status_is_clipped_to_the_texture() {
        let (width, height, stride) = (64, 64, 64 * 4);
        let mut pixels = vec![0; (stride * height) as usize];
        paint_status(
            &mut pixels,
            stride,
            width,
            height,
            &"ERROR ".repeat(100),
            [255; 3],
        );
        assert_eq!(pixels.len(), (stride * height) as usize);
    }
}
