//! Graphics broker: translates Android's surface model directly to Wayland.
//!
//! # What this replaces
//!
//! On real Android, an app's pixels travel
//! `EGL/GLES → gralloc GraphicBuffer → BufferQueue → SurfaceFlinger → display`.
//! PenguDroid does **not** run SurfaceFlinger. Instead it collapses that chain:
//! a guest surface becomes a `wl_surface` + `xdg_toplevel`, and its pixels live
//! in an [`AshmemRegion`] (a memfd) that is handed to the guest as an fd and
//! simultaneously wrapped in a `wl_shm` buffer. The compositor reads the same
//! memory the guest writes — no copy through a display server.
//!
//! For buffer exchange with a full GPU guest we would use the
//! `linux-dmabuf`/`zwp_linux_dmabuf_v1` path with host Vulkan allocations (the
//! "Lepton-style" driver pass-through), which is the natural upgrade from
//! `wl_shm` here. The daemon already owns the allocation and the fd, so the
//! swap is localized to [`State::finalize_surface`].
//!
//! # Status
//!
//! Real Wayland client: connects to the compositor, binds `wl_compositor`,
//! `wl_shm`, `xdg_wm_base`, creates toplevels, and presents a placeholder frame.
//! Guest pixel upload into the shared buffer is wired (the guest receives the
//! fd) but the MVP daemon also paints the frame itself so a window is visibly
//! produced end-to-end.

/// Describes a surface back to the control-socket peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceInfo {
    pub id: u32,
    /// Name of the backing ashmem region (also used as the memfd name).
    pub shm_name: String,
    pub stride: u32,
    pub size: u64,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

#[cfg(feature = "wayland")]
mod imp {
    use super::SurfaceInfo;
    use crate::ashmem::AshmemRegion;
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use sctk_adwaita::{AdwaitaFrame, FrameConfig};
    use smithay_client_toolkit::{compositor::{CompositorState, SurfaceData},
        subcompositor::SubcompositorState, shm::Shm, shell::WaylandSurface};
    use smithay_client_toolkit::reexports::csd_frame::{DecorationsFrame, FrameAction, FrameClick, WindowState, CursorIcon, WindowManagerCapabilities};
    use wayland_protocols::wp::viewporter::client::{wp_viewporter, wp_viewport};
    use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
    use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_manager_v1, wp_cursor_shape_device_v1};
    use wayland_protocols::wp::pointer_constraints::zv1::client::{
        zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
    };
    use wayland_protocols::wp::relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
    };
    use std::collections::HashMap;
    use std::os::fd::AsFd;

    use pengudroid_proto::{abi, BufferCrop};
    use pengudroid_proto::vsync_input_event;
    use crate::wlegl::protocol::imp::android_wlegl::AndroidWlegl;
    use crate::wlegl::protocol::imp::android_wlegl_handle::AndroidWleglHandle;
    use std::time::Instant;
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_region,
        wl_touch,
        wl_seat, wl_shell, wl_shell_surface, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
    };
    use crate::qt_extended_surface::imp::{qt_extended_surface, qt_surface_extension};
    use std::ffi::c_void;
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
    };
    use wayland_protocols::xdg::decoration::zv1::client::{
        zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
    };
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    /// A mapped toplevel plus the memory backing it.
    #[allow(dead_code)] // `configured`/`closed` are diagnostics today.
    pub struct Surface {
        pub wl_surface: wl_surface::WlSurface,
        /// `None` for a subsurface: only a toplevel has an `xdg_surface`.
        pub xdg_surface: Option<xdg_surface::XdgSurface>,
        pub xdg_toplevel: Option<xdg_toplevel::XdgToplevel>,
        pub shell_surface: Option<wl_shell_surface::WlShellSurface>,
        /// Set when this surface is a child layer of another one.
        pub subsurface: Option<wl_subsurface::WlSubsurface>,
        pub parent_id: Option<u32>,
        pub buffer: wl_buffer::WlBuffer,
        /// Kept alive so the memfd is not closed while the compositor may read
        /// it, and so `present` can repaint.
        pub region: AshmemRegion,
        /// When the surface is fed by an imported dma-buf, its fd is kept alive
        /// here (the compositor holds its own reference, but the buffer's memory
        /// must stay valid while attached).
        pub dmabuf_fd: Option<std::os::fd::OwnedFd>,
        pending_frames: Vec<PendingFrame>,
        pub width: u32,
        pub height: u32,
        pub configured: bool,
        pub closed: bool,
        /// Until the first guest frame arrives, the daemon animates a spinner
        /// in this surface instead of leaving the user on the system splash.
        loading: bool,
        pub fps_meter: bool,
        pub title: String,
        /// Guest pixels are independent of the compositor's content dimensions.
        pub display_width: u32,
        pub display_height: u32,
        pub viewport: Option<wp_viewport::WpViewport>,
        pub position: (i32, i32),
        pub window_state: WindowState,
        desktop_configured: bool,
    }

    /// Stable identity of a guest native handle across presents: the fds are
    /// duplicated on every frame, the int words are not.
    fn fnv1a_ints(ints: &[i32]) -> u64 {
        let mut hash = 0xcbf29ce484222325u64;
        for word in ints {
            for byte in word.to_le_bytes() {
                hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
            }
        }
        hash
    }

    fn ints_as_bytes(ints: &[i32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(ints.len() * 4);
        for word in ints {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out
    }

    struct PendingFrame {
        buffer: wl_buffer::WlBuffer,
        /// Direct PresentShm keeps the guest memfd alive until release.
        _fd: Option<std::os::fd::OwnedFd>,
        /// All fds of an Android native handle, held until `wl_buffer.release`.
        _native_fds: Vec<std::os::fd::OwnedFd>,
    }

    /// Per-surface compositor clock: one outstanding `wl_surface.frame`.
    struct VsyncState {
        callback: Option<wl_callback::WlCallback>,
        /// Last callback time in CLOCK_MONOTONIC nanoseconds.
        last_ns: i64,
        last_present_ns: i64,
        frames: u64,
    }

    struct ClipboardSource {
        proxy: wl_data_source::WlDataSource,
        text: String,
    }
    struct ClipboardOffer {
        proxy: wl_data_offer::WlDataOffer,
        mime_types: Vec<String>,
    }

    /// The broker's surface id, carried as a toplevel's user data so events can
    /// name their own window.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct SurfaceId(u32);

    /// A surface waiting for its first `xdg_surface.configure`.
    struct Pending {
        id: u32,
        width: u32,
        height: u32,
        xdg_surface: Option<xdg_surface::XdgSurface>,
        shell_surface: Option<wl_shell_surface::WlShellSurface>,
        wl_surface: wl_surface::WlSurface,
        /// Read through the region's fd on `finalize_surface`.
        region: AshmemRegion,
        stride: u32,
        title: String,
    }

    #[derive(Default)]
    struct OutputSize {
        pixels: (u32, u32),
        scale: u32,
        rotated: bool,
        logical: Option<(u32, u32)>,
    }

    fn fit_window(source: (u32, u32), bounds: (u32, u32)) -> (u32, u32) {
        let (w, h) = (source.0.max(1), source.1.max(1));
        let (max_w, max_h) = (bounds.0.max(1), bounds.1.max(1));
        if w <= max_w && h <= max_h { return (w, h); }
        if (w as u64 * max_h as u64) > (h as u64 * max_w as u64) {
            (max_w, (h as u64 * max_w as u64 / w as u64).max(1) as u32)
        } else {
            ((w as u64 * max_h as u64 / h as u64).max(1) as u32, max_h)
        }
    }

    struct FrameSurface(wl_surface::WlSurface);
    impl WaylandSurface for FrameSurface {
        fn wl_surface(&self) -> &wl_surface::WlSurface { &self.0 }
    }

    pub struct State {
        pub compositor: Option<wl_compositor::WlCompositor>,
        pub shm: Option<wl_shm::WlShm>,
        pub wm_base: Option<xdg_wm_base::XdgWmBase>,
        pub shell: Option<wl_shell::WlShell>,
        surface_extension: Option<qt_surface_extension::QtSurfaceExtension>,
        maliit: Option<crate::ime::Maliit>,
        ime_surface: Option<u32>,
        viewporter: Option<wp_viewporter::WpViewporter>,
        cursor_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
        cursor: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
        /// Mouse-look: the two protocols a pointer lock needs, the lock itself
        /// (and the surface it is on), and the relative-motion source.
        pointer_constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
        relative_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
        relative_pointer: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
        pointer_lock: Option<(u32, zwp_locked_pointer_v1::ZwpLockedPointerV1)>,
        pointer_serial: u32,
        output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
        outputs: HashMap<u32, OutputSize>,
        output_proxies: HashMap<u32, (wl_output::WlOutput, Option<zxdg_output_v1::ZxdgOutputV1>)>,
        frame_compositor: Option<Arc<CompositorState>>,
        frame_subcompositor: Option<Arc<SubcompositorState>>,
        frame_shm: Option<Shm>,
        frames: HashMap<u32, AdwaitaFrame<State>>,
        client_decorated: std::collections::HashSet<u32>,
        configured: HashMap<u32, (i32, i32, WindowState)>,
        wm_capabilities: HashMap<u32, WindowManagerCapabilities>,
        frame_focus: Option<(u32, wayland_client::backend::ObjectId)>,
        /// `zxdg_decoration_manager_v1`, when the compositor offers it.
        ///
        /// Compositors offering server-side decorations can supply a native
        /// title bar. GNOME/Mutter usually requires client-side decorations;
        /// that fallback must be supplied by this client, not the compositor.
        pub decoration: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
        /// `zwp_linux_dmabuf_v1`, when the compositor advertises it (v3+, which
        /// carries format modifiers).
        pub dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
        /// Sailfish/hybris `android_wlegl`: hand the compositor an Android
        /// native (gralloc) buffer as a GPU texture instead of a CPU wl_shm
        /// read. Lipstick advertises it; other compositors simply lack it.
        pub wlegl: Option<AndroidWlegl>,
        /// One native handle -> one wl_buffer for the surface's life (manual
        /// android_wlegl path, the ARM32 default).
        wlegl_buffers: HashMap<(u32, u64), (wl_buffer::WlBuffer, Vec<std::os::fd::OwnedFd>)>,
        /// Opt-in: the launcher asks for the native transport; the presence of
        /// the global alone only makes it possible.
        pub wlegl_enabled: bool,
        wlegl_name: Option<u32>,
        /// EGL presenter: owns EGLSurfaces and presents via eglSwapBuffers.
        egl_presenter: Option<crate::egl::Presenter>,
        /// `wl_subcompositor`, used for child layers (`SurfaceView` video).
        pub subcompositor: Option<wl_subcompositor::WlSubcompositor>,
        /// How many `format`/`modifier` events the compositor sent (diagnostic).
        pub dmabuf_formats: u32,
        /// The `(fourcc, modifier)` pairs the compositor advertised. v1-v2 send
        /// `format` only (modifier 0); v3+ send `modifier` events, and v4 may
        /// send *only* those, so both are recorded.
        pub dmabuf_supported: Vec<(u32, u64)>,
        /// Result flags for the dmabuf self-test.
        pub dmabuf_created: bool,
        pub dmabuf_failed: bool,
        next_id: u32,
        surfaces: HashMap<u32, Surface>,
        pending: Vec<Pending>,
        /// Toplevel proxies awaiting their surface's first configure.
        toplevels: HashMap<u32, xdg_toplevel::XdgToplevel>,
        /// Decoration proxies, kept alive for the surface's lifetime.
        decorations: HashMap<u32, zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
        extended_surfaces: HashMap<u32, qt_extended_surface::QtExtendedSurface>,
        /// Frames presented per surface, for the FPS meter:
        /// `(frames since the last update, when that was)`.
        fps: HashMap<u32, (u32, std::time::Instant)>,
        /// Current loading-stage or error text shown on the black placeholder.
        loading_status: String,
        /// Ids of surfaces the compositor asked us to close.
        pub close_requests: Vec<u32>,
        /// The seat, once the compositor advertises one.
        pub seat: Option<wl_seat::WlSeat>,
        data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
        data_device: Option<wl_data_device::WlDataDevice>,
        clipboard_sources: HashMap<wayland_client::backend::ObjectId, ClipboardSource>,
        clipboard_offers: HashMap<wayland_client::backend::ObjectId, ClipboardOffer>,
        clipboard_offer: Option<wayland_client::backend::ObjectId>,
        clipboard_contents: Option<String>,
        clipboard_read_cache: Option<String>,
        clipboard_selection_serial: Option<u32>,
        pub pointer: Option<wl_pointer::WlPointer>,
        pub touch: Option<wl_touch::WlTouch>,
        touch_points: HashMap<i32, u32>,
        pub keyboard: Option<wl_keyboard::WlKeyboard>,
        /// Surface under the pointer, and the last position in surface pixels.
        pointer_focus: Option<u32>,
        keyboard_focus: Option<u32>,
        held_keys: std::collections::HashSet<u32>,
        pointer_pos: (f64, f64),
        /// Input waiting to be forwarded to the guest that owns the surface.
        pub input: Vec<(u32, pengudroid_proto::InputKind)>,
        /// Encoded compositor-clock pulses, delivered as-is (no re-encoding).
        pub raw_vsync: Vec<(u32, Vec<u8>)>,
        /// A compositor frame callback per surface: the display clock the
        /// guest paces its rendering by, rather than a free-running timer.
        vsync: HashMap<u32, VsyncState>,
        /// Measured frame interval, for the guest's vsync timeline.
        vsync_interval_ns: i64,
        vsync_enabled: bool,
        vsync_stats: (u32, Instant),
        vsync_enabled_explicit: bool,
    }

    impl Default for State {
        fn default() -> Self {
            State {
                compositor: None,
                shm: None,
                wm_base: None,
                shell: None,
                surface_extension: None,
                maliit: None,
                ime_surface: None,
                viewporter: None,
                cursor_manager: None,
                cursor: None,
                pointer_constraints: None,
                relative_manager: None,
                relative_pointer: None,
                pointer_lock: None,
                pointer_serial: 0,
                output_manager: None,
                outputs: HashMap::new(),
                output_proxies: HashMap::new(),
                frame_compositor: None,
                frame_subcompositor: None,
                frame_shm: None,
                frames: HashMap::new(),
                client_decorated: Default::default(),
                configured: HashMap::new(),
                wm_capabilities: HashMap::new(),
                frame_focus: None,
                decoration: None,
                dmabuf: None,
                wlegl: None,
                wlegl_buffers: HashMap::new(),
                wlegl_enabled: false,
                wlegl_name: None,
                egl_presenter: None,
                subcompositor: None,
                dmabuf_formats: 0,
                dmabuf_supported: Vec::new(),
                dmabuf_created: false,
                dmabuf_failed: false,
                next_id: 0,
                surfaces: HashMap::new(),
                pending: Vec::new(),
                toplevels: HashMap::new(),
                decorations: HashMap::new(),
                extended_surfaces: HashMap::new(),
                fps: HashMap::new(),
                loading_status: crate::window::STARTING_STATUS.to_string(),
                close_requests: Vec::new(),
                seat: None,
                data_device_manager: None,
                data_device: None,
                clipboard_sources: HashMap::new(),
                clipboard_offers: HashMap::new(),
                clipboard_offer: None,
                clipboard_contents: None,
                clipboard_read_cache: None,
                clipboard_selection_serial: None,
                pointer: None,
                touch: None,
                touch_points: HashMap::new(),
                keyboard: None,
                pointer_focus: None,
                keyboard_focus: None,
                held_keys: Default::default(),
                pointer_pos: (0.0, 0.0),
                input: Vec::new(),
                raw_vsync: Vec::new(),
                vsync: HashMap::new(),
                vsync_interval_ns: 16_666_666,
                vsync_enabled: false,
                vsync_stats: (0, Instant::now()),
                vsync_enabled_explicit: false,
            }
        }
    }

    impl State {
        /// Send a compositor-timed vsync pulse to the surface's subscribers.
        /// The compositor's frame edge, on CLOCK_MONOTONIC.
        pub fn monotonic_ns() -> i64 {
            let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
            ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
        }

        /// One frame callback completed: timestamp it, measure the interval,
        /// and queue the next one. The pulse goes to the surface's subscribers
        /// as a raw encoded frame so its clock is never rewritten.
        pub fn on_vsync_done(&mut self, id: u32, _callback_data: u32) {
            let now = Self::monotonic_ns();
            let mut send = None;
            if let Some(state) = self.vsync.get_mut(&id) {
                state.callback = None;
                let previous = state.last_ns;
                state.last_ns = now;
                state.frames += 1;
                if previous != 0 {
                    let delta = now - previous;
                    if (1_000_000..=1_000_000_000).contains(&delta) {
                        self.vsync_interval_ns = delta;
                    }
                }
                let interval = self.vsync_interval_ns.max(1_000_000);
                send = Some(vsync_input_event(id, now, interval));
            }
            if let Some(frame) = send {
                self.raw_vsync.push((id, frame));
            }
            self.vsync_stats.0 += 1;
            let elapsed = self.vsync_stats.1.elapsed();
            if elapsed.as_secs() >= 5 {
                crate::info!(
                    "compositor vsync: {} callbacks/5s, interval {} ns, surfaces {}",
                    self.vsync_stats.0,
                    self.vsync_interval_ns,
                    self.vsync.len()
                );
                self.vsync_stats = (0, Instant::now());
            }
        }

        fn alloc_id(&mut self) -> u32 {
            self.next_id += 1;
            self.next_id
        }

        /// Which broker surface a `wl_surface` belongs to.
        fn surface_id_of(&self, s: &wl_surface::WlSurface) -> Option<u32> {
            self.surfaces
                .iter()
                .find(|(_, v)| v.wl_surface.id() == s.id())
                .map(|(id, _)| *id)
        }

        fn release_pointer_lock(&mut self) {
            if let Some((id, lock)) = self.pointer_lock.take() {
                lock.destroy();
                crate::info!("pointer lock on surface {id} released");
            }
        }

        fn push_input(&mut self, event: pengudroid_proto::InputKind) {
            if let Some(id) = self.pointer_focus {
                self.input.push((id, event));
            }
        }

        fn keyboard_key(&mut self, keycode: u32, pressed: bool) {
            if pressed { self.held_keys.insert(keycode); } else { self.held_keys.remove(&keycode); }
            if let Some(id) = self.keyboard_focus {
                self.input.push((id, pengudroid_proto::InputKind::Key { keycode, pressed }));
            }
        }

        fn release_keyboard(&mut self) {
            if let Some(id) = self.keyboard_focus {
                for keycode in self.held_keys.drain() {
                    self.input.push((id, pengudroid_proto::InputKind::Key { keycode, pressed: false }));
                }
            }
            self.held_keys.clear();
            self.keyboard_focus = None;
        }

        fn hide_ime_surface(&mut self, id: u32, reason: &'static str) {
            if self.ime_surface != Some(id) { return; }
            if let Some(maliit) = self.maliit.as_mut() {
                let _ = maliit.set_visible(false);
            }
            self.ime_surface = None;
            // Also clear the guest's actual-panel state. Android editor focus
            // deliberately remains intact and must not reopen it by itself.
            self.input.push((id, pengudroid_proto::InputKind::FinishComposing));
            crate::info!("surface {id} IME visible=false ({reason})");
        }

        fn request_close(&mut self, id: u32) {
            if let Some(surface) = self.surfaces.get_mut(&id) {
                if !surface.closed {
                    surface.closed = true;
                    self.close_requests.push(id);
                }
            }
        }

        fn redraw_frame(&mut self, id: u32) {
            if let Some(frame) = self.frames.get_mut(&id) {
                if frame.is_dirty() { frame.draw(); }
            }
            if let Some(surface) = self.surfaces.get(&id) { surface.wl_surface.commit(); }
        }

        fn scale_children(&mut self, parent: u32) {
            let Some(root) = self.surfaces.get(&parent) else { return };
            let (source, display) = ((root.width, root.height), (root.display_width, root.display_height));
            for surface in self.surfaces.values_mut().filter(|s| s.parent_id == Some(parent)) {
                let scale = |value: u32, axis: usize| -> u32 {
                    let (from, to) = if axis == 0 { (source.0, display.0) } else { (source.1, display.1) };
                    ((value as u64 * to as u64) / from.max(1) as u64).max(1) as u32
                };
                surface.display_width = scale(surface.width, 0);
                surface.display_height = scale(surface.height, 1);
                if let Some(viewport) = &surface.viewport {
                    viewport.set_destination(surface.display_width as i32, surface.display_height as i32);
                }
                if let Some(sub) = &surface.subsurface {
                    sub.set_position(
                        (surface.position.0 as i64 * display.0 as i64 / source.0.max(1) as i64) as i32,
                        (surface.position.1 as i64 * display.1 as i64 / source.1.max(1) as i64) as i32,
                    );
                }
                surface.wl_surface.commit();
            }
        }

        fn configure_window(&mut self, id: u32, qh: &QueueHandle<Self>) {
            let (width, height, flags) = self.configured.remove(&id).unwrap_or((0, 0, WindowState::empty()));
            let Some(surface) = self.surfaces.get(&id) else { return };
            if self.client_decorated.contains(&id) && !self.frames.contains_key(&id) {
                if let (Some(shm), Some(compositor), Some(subcompositor)) =
                    (&self.frame_shm, &self.frame_compositor, &self.frame_subcompositor) {
                    match AdwaitaFrame::new(&FrameSurface(surface.wl_surface.clone()), shm,
                        Arc::clone(compositor), Arc::clone(subcompositor), qh.clone(), FrameConfig::auto()) {
                        Ok(mut frame) => {
                            frame.set_title(surface.title.clone());
                            frame.set_resizable(self.viewporter.is_some());
                            let mut capabilities = self.wm_capabilities.get(&id).copied().unwrap_or(WindowManagerCapabilities::all());
                            if self.viewporter.is_none() { capabilities.remove(WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::FULLSCREEN); }
                            frame.update_wm_capabilities(capabilities);
                            self.frames.insert(id, frame);
                            crate::info!("host client-side frame created for surface {id}");
                        }
                        Err(e) => crate::warn!("cannot create desktop frame for {id}: {e}"),
                    }
                }
            }
            let (mut content_w, mut content_h) = (surface.display_width, surface.display_height);
            // Start portrait Android windows inside the desktop instead of
            // putting their bottom controls below the monitor's visible area.
            if !surface.desktop_configured && surface.viewport.is_some() && width <= 0 && height <= 0 {
                let logical: Vec<_> = self.outputs.values().filter_map(|output| {
                    if let Some(size) = output.logical { return Some(size); }
                    let (w, h) = if output.rotated { (output.pixels.1, output.pixels.0) } else { output.pixels };
                    (w > 0 && h > 0).then_some((w / output.scale.max(1), h / output.scale.max(1)))
                }).collect();
                if let (Some(max_w), Some(max_h)) = (logical.iter().map(|(w, _)| *w).min(), logical.iter().map(|(_, h)| *h).min()) {
                    let header = self.frames.get(&id).map(|frame| frame.add_borders(0, 0).1).unwrap_or(0);
                    (content_w, content_h) = fit_window((content_w, content_h),
                        ((max_w * 85 / 100).max(1), (max_h * 85 / 100).saturating_sub(header).max(1)));
                }
            }
            if let Some(frame) = self.frames.get_mut(&id) {
                frame.update_state(flags);
                frame.set_hidden(flags.contains(WindowState::FULLSCREEN));
                if let (Some(w), Some(h)) = (NonZeroU32::new(width.max(0) as u32), NonZeroU32::new(height.max(0) as u32)) {
                    let (w, h) = frame.subtract_borders(w, h);
                    if let Some(w) = w { content_w = w.get(); }
                    if let Some(h) = h { content_h = h.get(); }
                } else {
                    if width > 0 { content_w = width as u32; }
                    if height > 0 {
                        let header = frame.add_borders(0, 0).1;
                        content_h = (height as u32).saturating_sub(header).max(1);
                    }
                }
                if surface.viewport.is_none() { content_w = surface.width; content_h = surface.height; }
                if !frame.is_hidden() {
                    frame.resize(NonZeroU32::new(content_w.max(1)).unwrap(), NonZeroU32::new(content_h.max(1)).unwrap());
                }
            } else {
                if width > 0 { content_w = width as u32; }
                if height > 0 { content_h = height as u32; }
            }
            let surface = self.surfaces.get_mut(&id).unwrap();
            surface.window_state = flags;
            surface.desktop_configured = true;
            if let Some(viewport) = &surface.viewport {
                surface.display_width = content_w.max(1);
                surface.display_height = content_h.max(1);
                viewport.set_destination(surface.display_width as i32, surface.display_height as i32);
            }
            let (x, y, w, h) = match self.frames.get(&id) {
                Some(frame) => {
                    let (x, y) = frame.location();
                    let (w, h) = frame.add_borders(surface.display_width, surface.display_height);
                    (x, y, w, h)
                }
                None => (0, 0, surface.display_width, surface.display_height),
            };
            if let Some(xdg) = &surface.xdg_surface { xdg.set_window_geometry(x, y, w as i32, h as i32); }
            if surface.viewport.is_none() {
                if let Some(top) = &surface.xdg_toplevel {
                    top.set_min_size(w as i32, h as i32); top.set_max_size(w as i32, h as i32);
                }
            }
            self.scale_children(id);
            self.redraw_frame(id);
        }

        fn guest_coordinates(&self, id: u32, x: f64, y: f64) -> (f32, f32) {
            let Some(surface) = self.surfaces.get(&id) else { return (x as f32, y as f32) };
            (x as f32 * surface.width as f32 / surface.display_width.max(1) as f32,
             y as f32 * surface.height as f32 / surface.display_height.max(1) as f32)
        }

        fn set_cursor(&self, icon: CursorIcon) {
            use wp_cursor_shape_device_v1::Shape as S;
            use CursorIcon as C;
            let shape = match icon {
                C::NResize => S::NResize, C::SResize => S::SResize,
                C::EResize => S::EResize, C::WResize => S::WResize,
                C::NeResize => S::NeResize, C::NwResize => S::NwResize,
                C::SeResize => S::SeResize, C::SwResize => S::SwResize,
                C::EwResize => S::EwResize, C::NsResize => S::NsResize,
                C::NeswResize => S::NeswResize, C::NwseResize => S::NwseResize,
                _ => S::Default,
            };
            if let Some(cursor) = &self.cursor { cursor.set_shape(self.pointer_serial, shape); }
        }

        fn update_frame_title(&mut self, id: u32, title: String) {
            if let Some(surface) = self.surfaces.get(&id) {
                if let Some(top) = &surface.xdg_toplevel { top.set_title(title.clone()); }
            }
            if let Some(frame) = self.frames.get_mut(&id) { frame.set_title(title); }
            self.redraw_frame(id);
        }

        fn frame_enter(&mut self, surface: &wl_surface::WlSurface, x: f64, y: f64) -> bool {
            let mut found = None;
            for (id, frame) in &mut self.frames {
                if let Some(cursor) = frame.click_point_moved(std::time::Duration::ZERO, &surface.id(), x, y) {
                    found = Some((*id, cursor)); break;
                }
            }
            if let Some((id, cursor)) = found {
                self.set_cursor(cursor);
                self.frame_focus = Some((id, surface.id()));
                self.redraw_frame(id);
                true
            } else { false }
        }

        fn frame_motion(&mut self, time: u32, x: f64, y: f64) -> bool {
            let Some((id, surface)) = self.frame_focus.clone() else { return false };
            let cursor = self.frames.get_mut(&id).and_then(|frame|
                frame.click_point_moved(std::time::Duration::from_millis(time as u64), &surface, x, y));
            if let Some(cursor) = cursor { self.set_cursor(cursor); }
            self.redraw_frame(id);
            true
        }

        fn frame_button(&mut self, button: u32, pressed: bool, time: u32, serial: u32) -> bool {
            let Some((id, _)) = &self.frame_focus else { return false };
            let id = *id;
            let click = match button { 0x110 => FrameClick::Normal, 0x111 => FrameClick::Alternate, _ => return true };
            let action = self.frames.get_mut(&id).and_then(|frame|
                frame.on_click(std::time::Duration::from_millis(time as u64), click, pressed));
            if let Some(action) = action {
                if matches!(action, FrameAction::Close) { self.request_close(id); }
                else if let (Some(surface), Some(seat)) = (self.surfaces.get(&id), &self.seat) {
                    if let Some(top) = &surface.xdg_toplevel {
                        match action {
                            FrameAction::Move => top._move(seat, serial),
                            FrameAction::Minimize => top.set_minimized(),
                            FrameAction::Maximize => top.set_maximized(),
                            FrameAction::UnMaximize => top.unset_maximized(),
                            FrameAction::ShowMenu(x, y) => top.show_window_menu(seat, serial, x, y),
                            FrameAction::Resize(edge) => {
                                use smithay_client_toolkit::reexports::csd_frame::ResizeEdge as E;
                                let edge = match edge {
                                    E::Top => xdg_toplevel::ResizeEdge::Top, E::Bottom => xdg_toplevel::ResizeEdge::Bottom,
                                    E::Left => xdg_toplevel::ResizeEdge::Left, E::Right => xdg_toplevel::ResizeEdge::Right,
                                    E::TopLeft => xdg_toplevel::ResizeEdge::TopLeft, E::TopRight => xdg_toplevel::ResizeEdge::TopRight,
                                    E::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft, E::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
                                    _ => xdg_toplevel::ResizeEdge::None,
                                };
                                top.resize(seat, serial, edge);
                            }
                            _ => {}
                        }
                    }
                }
            }
            self.redraw_frame(id);
            true
        }

        fn pump_maliit(&mut self) {
            use pengudroid_proto::InputKind;
            let Some(maliit) = self.maliit.as_mut() else { return };
            let events = maliit.drain_events();
            let Some(surface) = self.ime_surface else { return };
            let mut deactivated = false;
            for event in events {
                match event {
                    crate::ime::ImeEvent::Commit(text) => {
                        self.input.push((surface, InputKind::CommitText { text }));
                    }
                    crate::ime::ImeEvent::Preedit(text) => {
                        self.input.push((surface, InputKind::PreeditText { text }));
                    }
                    crate::ime::ImeEvent::FinishComposing => {
                        self.input.push((surface, InputKind::FinishComposing));
                        // Self-initiated hide must also disarm the panel touch
                        // filter. Keeping only Maliit's hidden flag preserves
                        // explicit re-show for an already focused editor.
                        deactivated = true;
                    }
                    crate::ime::ImeEvent::Key { key, text } => {
                        let clipboard = match key as u32 {
                            0x010000cf => Some(0), // Qt::Key_Copy
                            0x010000d0 => Some(1), // Qt::Key_Cut
                            0x010000e2 => Some(2), // Qt::Key_Paste
                            _ => None,
                        };
                        if let Some(action) = clipboard {
                            self.input.push((surface, InputKind::ClipboardAction { action }));
                            continue;
                        }
                        // Special keys take precedence even when Qt also
                        // supplies a control character as text (e.g. "\b").
                        let linux = match key as u32 {
                            0x01000000 => Some(1),   // Escape
                            0x01000001 => Some(15),  // Tab
                            0x01000003 => Some(14),  // Backspace
                            0x01000004 | 0x01000005 => Some(28), // Return/Enter
                            0x01000007 => Some(111), // Delete
                            0x01000012 => Some(105), // Left
                            0x01000013 => Some(103), // Up
                            0x01000014 => Some(106), // Right
                            0x01000015 => Some(108), // Down
                            _ => None,
                        };
                        if let Some(keycode) = linux {
                            self.input.push((surface, InputKind::Key { keycode, pressed: true }));
                            self.input.push((surface, InputKind::Key { keycode, pressed: false }));
                        } else if !text.is_empty() {
                            self.input.push((surface, InputKind::CommitText { text }));
                        }
                    }
                }
            }
            if deactivated { self.ime_surface = None; }
        }

        /// Repaint the black loading screen with the current status text.
        /// Replaces the old spinner: the user sees the current boot stage or
        /// the error that stopped the launch.
        fn repaint_loading(&mut self) {
            // On AArch64 Aurora the EGL-presented surface receives no wl_shm
            // content at all. Older ARM32 Lipstick builds close a window that
            // never commits, so there the wl_shm loading screen stays alive
            // until the first native-buffer frame (surface.loading clears then).
            if self.wlegl_enabled && !cfg!(target_arch = "arm") {
                return; // no wl_shm loading buffer on the EGL-presented surface
            }
            let status = self.loading_status.clone();
            let ids: Vec<u32> = self.surfaces.iter()
                .filter_map(|(id, s)| (s.loading && s.configured && !s.closed).then_some(*id))
                .collect();
            for id in ids {
                let Some(surface) = self.surfaces.get_mut(&id) else { continue };
                let (w, h, stride) = (surface.width, surface.height, (surface.width * 4) as i32);
                if let Ok(mut map) = surface.region.map_mut() {
                    crate::window::paint_status(
                        map.as_mut_slice(),
                        stride as u32,
                        w,
                        h,
                        &status,
                        [200, 200, 200],
                    );
                }
                surface.wl_surface.attach(Some(&surface.buffer), 0, 0);
                surface.wl_surface.damage(0, 0, w as i32, h as i32);
                surface.wl_surface.commit();
            }
        }

        /// Update the loading-stage or error text and repaint the placeholder.
        pub(crate) fn set_loading_status(&mut self, text: String) {
            if self.loading_status == text { return; }
            self.loading_status = text;
            self.repaint_loading();
        }

        /// Build the `wl_shm` buffer for a pending surface once the compositor
        /// has configured it.
        ///
        /// The buffer is a read-only view of the surface's fd, which the guest
        /// also maps. Holding it in a `foreign` slot keeps it alive even after
        /// the guest (and this process's own handle) goes away, so the memfd
        /// itself is not released until the compositor has the newest frame.
        fn finalize_surface(&mut self, id: u32, qh: &QueueHandle<State>) {
            let Some(idx) = self.pending.iter().position(|p| p.id == id) else {
                return;
            };
            let p = self.pending.remove(idx);
            let Some(shm) = self.shm.as_ref() else { return };

            // Paint the current boot stage or error before the first guest frame.
            if let Ok(mut map) = p.region.map_mut() {
                crate::window::paint_status(
                    map.as_mut_slice(),
                    p.stride,
                    p.width,
                    p.height,
                    &self.loading_status,
                    [160, 160, 160],
                );
            }

            let dup = match p.region.try_clone_fd() {
                Ok(fd) => fd,
                Err(e) => {
                    crate::warn!("surface {}: cannot dup shm fd: {e}", p.id);
                    return;
                }
            };
            let pool = shm.create_pool(dup.as_fd(), p.region.size() as i32, qh, ());
            // The buffer's user data is unused (we keep our own handle below).
            let buffer = pool.create_buffer(
                0,
                p.width as i32,
                p.height as i32,
                p.stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (),
            );
            pool.destroy();



            if !self.wlegl_enabled || cfg!(target_arch = "arm") {
                p.wl_surface.attach(Some(&buffer), 0, 0);
                p.wl_surface.damage(0, 0, p.width as i32, p.height as i32);
                if let Some(xdg) = p.xdg_surface.as_ref() {
                    xdg.set_window_geometry(0, 0, p.width as i32, p.height as i32);
                }
                p.wl_surface.commit();
            }

            self.surfaces.insert(
                p.id,
                Surface {
                    wl_surface: p.wl_surface.clone(),
                    xdg_surface: p.xdg_surface,
                    xdg_toplevel: self.toplevels.remove(&p.id),
                    shell_surface: p.shell_surface,
                    subsurface: None,
                    parent_id: None,
                    buffer,
                    region: p.region,
                    dmabuf_fd: None,
                    pending_frames: Vec::new(),
                    width: p.width,
                    height: p.height,
                    configured: true,
                    closed: false,
                    loading: true,
                    fps_meter: false,
                    title: p.title,
                    display_width: p.width,
                    display_height: p.height,
                    viewport: self.viewporter.as_ref().map(|v| v.get_viewport(&p.wl_surface, qh, ())),
                    position: (0, 0),
                    window_state: WindowState::empty(),
                    desktop_configured: false,
                },
            );
            crate::info!("surface {} configured ({}×{})", p.id, p.width, p.height);
        }
    }

    /// The broker. Owns the Wayland connection and its event queue.
    pub struct Graphics {
        conn: Connection,
        queue: EventQueue<State>,
        state: State,
    }

    impl Graphics {
        /// Tear down a toplevel whose `configure` never arrived, so its proxies
        /// do not leak and its id is not reused while half-alive.
        fn abandon_pending(&mut self, id: u32) {
            self.state.frames.remove(&id);
            self.state.extended_surfaces.remove(&id);
            self.state.client_decorated.remove(&id);
            self.state.configured.remove(&id);
            self.state.wm_capabilities.remove(&id);
            if let Some(decoration) = self.state.decorations.remove(&id) { decoration.destroy(); }
            if let Some(t) = self.state.toplevels.remove(&id) { t.destroy(); }
            if let Some(pos) = self.state.pending.iter().position(|p| p.id == id) {
                let p = self.state.pending.remove(pos);
                if let Some(xdg) = p.xdg_surface { xdg.destroy(); }
                // wl_shell_surface has no destroy request; dropping the proxy
                // together with wl_surface releases the legacy role.
                p.wl_surface.destroy();
            }
            let _ = self.conn.flush();
        }

        /// Connect to the compositor named by `WAYLAND_DISPLAY`. Returns `None`
        /// if there is no Wayland session (the daemon then runs graphics-less).
        /// `wl_shm`/`android_wlegl` choice and the frame clock are environment
        /// switches so an unverified transport never silently becomes the only
        /// path: the launcher opts in, and wl_shm remains available.
        fn env_enabled(name: &str) -> bool {
            std::env::var(name).ok().is_some_and(|v| v != "0" && !v.is_empty())
        }

        fn hybris_host() -> bool {
            let egl = if cfg!(target_arch = "arm") {
                "/usr/libexec/droid-hybris/system/lib/libEGL.so"
            } else {
                "/usr/libexec/droid-hybris/system/lib64/libEGL.so"
            };
            std::path::Path::new("/usr/lib/libhybris-common.so.1").is_file()
                && std::path::Path::new(egl).is_file()
        }

        pub fn connect() -> Option<Graphics> {
            let conn = match Connection::connect_to_env() {
                Ok(c) => c,
                Err(e) => {
                    crate::warn!("no Wayland session ({e}); graphics disabled");
                    return None;
                }
            };
            let (globals, mut queue) = match registry_queue_init::<State>(&conn) {
                Ok(v) => v,
                Err(e) => {
                    crate::warn!("Wayland registry init failed: {e}");
                    return None;
                }
            };
            let qh = queue.handle();
            let mut state = State::default();
            state.compositor = globals.bind(&qh, 1..=6, ()).ok();
            state.shm = globals.bind(&qh, 1..=1, ()).ok();
            state.wm_base = globals.bind(&qh, 1..=6, ()).ok();
            // Lipstick on Aurora still exposes the original wl_shell rather
            // than xdg-shell. It needs no initial configure roundtrip.
            state.shell = globals.bind(&qh, 1..=1, ()).ok();
            // Optional Lipstick integration; wl_shell/xdg remains the role.
            state.surface_extension = globals.bind(&qh, 1..=1, ()).ok();
            if state.surface_extension.is_some() {
                crate::info!("qt_surface_extension enabled");
            }
            state.decoration = globals.bind(&qh, 1..=1, ()).ok();
            if state.decoration.is_some() {
                crate::info!("server-side decorations available (xdg-decoration)");
            } else {
                crate::info!("no xdg-decoration; client-side title bar support is required");
            }
            // v3 adds format modifiers, which we need to import GPU buffers.
            // Bind v3 for the format/modifier events. v4 requires the feedback
            // protocol instead and no longer guarantees these advertisements.
            // The daemon may start before the launcher applies guest env.
            // Default to native presentation on Hybris itself; explicit 0
            // still forces the wl_shm fallback on either side.
            state.wlegl_enabled = match std::env::var("PENGUDROID_GRALLOC_WLEGL") {
                Ok(_) => Self::env_enabled("PENGUDROID_GRALLOC_WLEGL"),
                Err(_) => {
                    cfg!(any(target_arch = "aarch64", target_arch = "arm"))
                        && Self::hybris_host()
                }
            };
            state.vsync_enabled = match std::env::var("PENGUDROID_VSYNC_COMPOSITOR") {
                Ok(value) => value != "0" && !value.is_empty(),
                // Auto: on where the native-buffer transport exists (Lipstick).
                Err(_) => true,
            };
            state.vsync_enabled_explicit = state.vsync_enabled;
            state.wlegl_name = globals.contents().with_list(|list| {
                list.iter()
                    .find(|g| g.interface == "android_wlegl")
                    .map(|g| g.name)
            });
            state.vsync_stats = (0, Instant::now());
            state.dmabuf = globals.bind(&qh, 3..=3, ()).ok();
            state.subcompositor = globals.bind(&qh, 1..=1, ()).ok();
            state.viewporter = globals.bind(&qh, 1..=1, ()).ok();
            state.cursor_manager = globals.bind(&qh, 1..=1, ()).ok();
            state.pointer_constraints = globals.bind(&qh, 1..=1, ()).ok();
            state.relative_manager = globals.bind(&qh, 1..=1, ()).ok();
            state.frame_compositor = CompositorState::bind(&globals, &qh).ok().map(Arc::new);
            if let Some(compositor) = state.frame_compositor.as_ref() {
                state.frame_subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh).ok().map(Arc::new);
            }
            state.frame_shm = state.shm.clone().map(Shm::from);
            state.output_manager = globals.bind(&qh, 1..=3, ()).ok();
            globals.contents().with_list(|list| {
                for name in ["wl_drm", "zwp_linux_dmabuf_v1", "qt_surface_extension", "android_wlegl"] {
                    if let Some(global) = list.iter().find(|global| global.interface == name) {
                        crate::info!("Wayland global {name} v{}", global.version);
                    }
                }
            });
            if let Some(name) = state.wlegl_name {
                let wlegl: AndroidWlegl = globals.registry().bind(name, 1, &qh, ());
                state.wlegl = Some(wlegl);
                crate::info!(
                    "android_wlegl available; guest native-buffer transport {}",
                    if state.wlegl_enabled { "enabled" } else { "available but disabled" }
                );
            } else if state.wlegl_enabled {
                crate::warn!("compositor lacks android_wlegl; using wl_shm");
                state.wlegl_enabled = false;
            }
            globals.contents().with_list(|list| {
                for global in list.iter().filter(|global| global.interface == "wl_output") {
                    let output: wl_output::WlOutput = globals.registry().bind(global.name, global.version.min(4), &qh, global.name);
                    let logical = state.output_manager.as_ref().map(|manager| manager.get_xdg_output(&output, &qh, global.name));
                    state.outputs.insert(global.name, OutputSize::default());
                    state.output_proxies.insert(global.name, (output, logical));
                }
            });
            // Input: the seat gives us pointer and keyboard events, which are
            // forwarded to whichever guest owns the surface under the cursor.
            state.seat = globals.bind(&qh, 1..=7, ()).ok();
            state.data_device_manager = globals.bind(&qh, 1..=1, ()).ok();
            if let (Some(manager), Some(seat)) = (state.data_device_manager.as_ref(), state.seat.as_ref()) {
                state.data_device = Some(manager.get_data_device(seat, &qh, ()));
                crate::info!("Wayland clipboard data-device enabled");
            }
            if queue.roundtrip(&mut state).is_err() {
                crate::warn!("Wayland roundtrip failed");
                return None;
            }
            if state.compositor.is_none() || state.shm.is_none()
                || (state.wm_base.is_none() && state.shell.is_none()) {
                crate::warn!(
                    "Wayland missing globals (compositor={}, shm={}, xdg_wm_base={}, wl_shell={})",
                    state.compositor.is_some(),
                    state.shm.is_some(),
                    state.wm_base.is_some(),
                    state.shell.is_some()
                );
                return None;
            }
            crate::info!("Wayland graphics broker ready");
            Some(Graphics { conn, queue, state })
        }

        /// Dispatch pending compositor events and return the input they carried.
        ///
        /// Called from the daemon's accept loop; the events are then pushed to
        /// the connection that subscribed for the surface.
        pub fn poll_input(&mut self) -> Vec<(u32, pengudroid_proto::InputKind)> {
            self.drain_wayland();
            self.state.pump_maliit();
            std::mem::take(&mut self.state.input)
        }

        /// Compositor-clock pulses awaiting delivery to guest subscribers.
        /// Whether the compositor clock is currently being forwarded.
        pub fn vsync_available(&self) -> bool {
            self.state.vsync_enabled
        }

        pub fn take_vsync_frames(&mut self) -> Vec<(u32, Vec<u8>)> {
            std::mem::take(&mut self.state.raw_vsync)
        }

        /// Read whatever the compositor has already sent, without blocking.
        ///
        /// `dispatch_pending` alone only replays events that a *previous* read
        /// put in the queue, so on its own it never sees anything new: the socket
        /// has to be read first. That was why pointer clicks did nothing — no
        /// `wl_pointer::Enter`/`Button` event ever reached the state machine, so
        /// `pointer_focus` stayed `None` and `push_input` dropped everything.
        ///
        fn vsync_enabled_now(&self) -> bool {
            // Auto mode: on only when the compositor advertises android_wlegl.
            // `PENGUDROID_VSYNC_COMPOSITOR=1` forces it anywhere (desktop).
            self.state.vsync_enabled
        }

        fn queue_vsync(&mut self, id: u32) {
            if !self.vsync_enabled_now() { return; }
            let qh = self.queue.handle();
            let Some((wl_surface, entry)) = (|| {
                let surface = self.state.surfaces.get(&id)?;
                let wl_surface = surface.wl_surface.clone();
                Some((wl_surface, id))
            })() else { return };
            let state = self.state.vsync.entry(entry).or_insert_with(|| VsyncState {
                callback: None,
                last_ns: 0,
                last_present_ns: 0,
                frames: 0,
            });
            if state.callback.is_some() { return; }
            state.callback = Some(wl_surface.frame(&qh, ()));
        }

        /// Record one present and ask the compositor for the next frame edge.
        fn vsync_note_present(&mut self, id: u32) {
            if !self.vsync_enabled_now() { return; }
            self.queue_vsync(id);
        }

        /// The read is non-blocking: the daemon loop must stay responsive, and a
        /// `WouldBlock` simply means there was nothing to read.
        fn drain_wayland(&mut self) {
            // Never block: `poll(2)` first, so a blocking client socket cannot
            // stall the daemon loop.
            let fd = std::os::fd::AsRawFd::as_raw_fd(&self.conn.backend().poll_fd());
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ready > 0 && pfd.revents & libc::POLLIN != 0 {
                if let Some(guard) = self.queue.prepare_read() {
                    if let Err(e) = guard.read() {
                        let would_block = matches!(
                            &e,
                            wayland_client::backend::WaylandError::Io(io)
                                if io.kind() == std::io::ErrorKind::WouldBlock
                        );
                        if !would_block {
                            crate::warn!("wayland read: {e}");
                        }
                    }
                }
            }
            let _ = self.queue.dispatch_pending(&mut self.state);
            let _ = self.conn.flush();
            // Re-arm the frame callbacks only after dispatch: creating one in
            // the middle of an event would recurse into the same state.
            if self.state.vsync_enabled {
                let ids: Vec<u32> = self.state.surfaces.keys().copied().collect();
                for id in ids {
                    self.queue_vsync(id);
                }
            }
        }

        /// The surface that currently has pointer focus, if any.
        ///
        /// Input injection needs it: the compositor is the only thing that
        /// normally decides which surface a pointer event belongs to.
        pub fn focused_surface(&self) -> Option<u32> {
            self.state.pointer_focus
        }

        /// The ids of every toplevel the compositor knows about.
        pub fn surface_ids(&self) -> Vec<u32> {
            self.state.surfaces.keys().copied().collect()
        }

        /// Create a toplevel-backed surface `width`×`height` with `title`.
        /// Current physical output mode for a surface whose launcher did not
        /// request a fixed size. Logical desktop sizes are deliberately not
        /// used: Android density is derived independently from the theme.
        pub fn output_surface_size(&self) -> Option<(u32, u32)> {
            self.state.outputs.values()
                .filter_map(|output| {
                    let (mut w, mut h) = output.pixels;
                    if output.rotated { std::mem::swap(&mut w, &mut h); }
                    (w > 0 && h > 0).then_some((w, h))
                })
                .max_by_key(|(w, h)| *w as u64 * *h as u64)
        }

        pub fn create_toplevel(
            &mut self,
            width: u32,
            height: u32,
            title: &str,
        ) -> Option<SurfaceInfo> {
            let (width, height) = (width.max(64), height.max(64));
            let stride = width * 4;
            let size = (stride * height) as u64;
            let name = format!("pengudroid-fb-{}", self.state.next_id + 1);
            let region = match AshmemRegion::create(&name, size) {
                Ok(r) => r,
                Err(e) => {
                    crate::warn!("cannot allocate surface memory: {e}");
                    return None;
                }
            };

            let compositor = self.state.compositor.clone()?;
            let qh = self.queue.handle();
            let id = self.state.alloc_id();
            let parts = crate::window::create_toplevel(
                &compositor,
                self.state.frame_compositor.as_deref(),
                self.state.wm_base.as_ref(),
                self.state.shell.as_ref(),
                self.state.decoration.as_ref(),
                self.state.surface_extension.as_ref(),
                &qh,
                SurfaceId(id),
                width,
                height,
                title,
                self.state.viewporter.is_some(),
            )?;
            let crate::window::ToplevelParts {
                wl_surface,
                xdg_surface,
                xdg_toplevel,
                shell_surface,
                decoration,
                extended,
            } = parts;
            let legacy_shell = shell_surface.is_some();

            if let Some(toplevel) = xdg_toplevel {
                // Prefer server-side framing where offered. Without it,
                // configure_window installs the host client-side title bar.
                if let Some(decoration) = decoration {
                    self.state.decorations.insert(id, decoration);
                } else {
                    self.state.client_decorated.insert(id);
                }
                self.state.toplevels.insert(id, toplevel);
            }
            if let Some(extended) = extended {
                self.state.extended_surfaces.insert(id, extended);
                crate::info!("surface {id} has qt_extended_surface");
            }

            self.state.pending.push(Pending {
                id,
                width,
                height,
                xdg_surface,
                shell_surface,
                wl_surface,
                region,
                stride,
                title: title.to_owned(),
            });
            // wl_shell has no initial configure handshake: attach immediately.
            if legacy_shell {
                self.state.finalize_surface(id, &qh);
            }

            // Drive the configure → ack → attach → commit cycle synchronously.
            if let Err(e) = self.queue.roundtrip(&mut self.state) {
                crate::warn!("roundtrip during create failed: {e}");
                self.abandon_pending(id);
                return None;
            }

            // Lipstick may leave a newly created wl_shell window represented
            // only by its cover until the client explicitly raises it.  Send
            // the request after the initial configure/attach/commit cycle: an
            // earlier raise can arrive before the compositor has a mapped
            // window.  The role itself was already selected above with
            // wl_shell_surface.set_toplevel (or xdg_surface.get_toplevel).
            if let Some(extended) = self.state.extended_surfaces.get(&id) {
                extended.raise();
                crate::info!("surface {id} requested qt_extended_surface.raise after toplevel mapping");
                if let Err(e) = self.queue.roundtrip(&mut self.state) {
                    crate::warn!("roundtrip after toplevel raise failed: {e}");
                }
            }

            if let Some(surface) = self.state.surfaces.get_mut(&id) { surface.title = title.to_owned(); }
            let info = SurfaceInfo {
                id,
                shm_name: self
                    .state
                    .surfaces
                    .get(&id)
                    .map(|s| s.region.name().to_string())
                    .unwrap_or(name),
                stride,
                size,
                format: abi::ANDROID_RGBA_8888,
                width,
                height,
            };
            if !self.state.surfaces.contains_key(&id) {
                crate::warn!("surface {id} never got configured");
                self.abandon_pending(id);
                return None;
            }
            Some(info)
        }

        pub fn clipboard_set(&mut self, text: String) -> Result<(), String> {
            if text.len() > 16 * 1024 * 1024 {
                return Err("clipboard text exceeds 16 MiB".into());
            }
            let serial = self.state.clipboard_selection_serial
                .ok_or("clipboard copy has no touch/pointer/keyboard selection serial")?;
            let manager = self.state.data_device_manager.as_ref()
                .ok_or("compositor has no wl_data_device_manager")?;
            let data_device = self.state.data_device.as_ref().ok_or("no clipboard data device")?;
            let qh = self.queue.handle();
            let source = manager.create_data_source(&qh, ());
            for mime in ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"] {
                source.offer(mime.to_string());
            }
            source.offer(format!("pid/{}", std::process::id()));
            self.state.clipboard_contents = Some(text.clone());
            self.state.clipboard_read_cache = Some(text.clone());
            self.state.clipboard_sources.insert(source.id(), ClipboardSource {
                proxy: source.clone(), text,
            });
            data_device.set_selection(Some(&source), serial);
            self.conn.flush().map_err(|e| e.to_string())?;
            crate::info!("clipboard source selected ({} MIME entries)", 4);
            Ok(())
        }

        pub fn clipboard_clear(&mut self) -> Result<(), String> {
            let serial = self.state.clipboard_selection_serial
                .ok_or("clipboard clear has no selection serial")?;
            let data_device = self.state.data_device.as_ref().ok_or("no clipboard data device")?;
            self.state.clipboard_contents = None;
            self.state.clipboard_read_cache = None;
            data_device.set_selection(None, serial);
            self.conn.flush().map_err(|e| e.to_string())?;
            Ok(())
        }

        pub fn clipboard_read(&mut self) -> Result<String, String> {
            use std::os::fd::{AsFd, AsRawFd, FromRawFd};
            self.drain_wayland();
            if let Some(cached) = self.state.clipboard_read_cache.as_ref() {
                return Ok(cached.clone());
            }
            let Some(offer_id) = self.state.clipboard_offer.clone() else {
                return self.state.clipboard_contents.clone().ok_or_else(|| "clipboard is empty".to_string());
            };
            let offer = self.state.clipboard_offers.get(&offer_id).ok_or("clipboard offer disappeared")?;
            let own_mime = format!("pid/{}", std::process::id());
            if offer.mime_types.iter().any(|mime| mime == &own_mime) {
                return self.state.clipboard_contents.clone().ok_or_else(|| "own clipboard is empty".to_string());
            }
            let mime = ["text/plain", "text/plain;charset=utf-8", "UTF8_STRING"]
                .iter().find(|mime| offer.mime_types.iter().any(|offered| offered == *mime))
                .ok_or("clipboard offer has no supported text MIME")?.to_string();
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                return Err(format!("clipboard pipe: {}", std::io::Error::last_os_error()));
            }
            let (read_fd, write_fd) = unsafe {
                (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1]))
            };
            offer.proxy.receive(mime, write_fd.as_fd());
            self.conn.flush().map_err(|e| e.to_string())?;
            // receive() owns the compositor-side duplicate; our write end must
            // close now so the reader can observe EOF.
            unsafe {
                let flags = libc::fcntl(read_fd.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(read_fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            let started = std::time::Instant::now();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0u8; 8192];
                let count = unsafe {
                    libc::read(read_fd.as_raw_fd(), chunk.as_mut_ptr() as *mut c_void, chunk.len())
                };
                if count > 0 {
                    bytes.extend_from_slice(&chunk[..count as usize]);
                    if bytes.len() > 16 * 1024 * 1024 {
                        return Err("clipboard text exceeds 16 MiB".into());
                    }
                    continue;
                }
                if count == 0 { break; }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(format!("clipboard read: {error}"));
                }
                if started.elapsed() >= std::time::Duration::from_millis(500) {
                    return Err("clipboard read timed out".into());
                }
                let mut poll = libc::pollfd { fd: read_fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                unsafe { libc::poll(&mut poll, 1, 20) };
            }
            let text = String::from_utf8(bytes).map_err(|_| "clipboard is not valid UTF-8".to_string())?
                .replace("\r\n", "\n");
            self.state.clipboard_read_cache = Some(text.clone());
            Ok(text)
        }

        /// Show/hide the system input method only when Android's focused View
        /// changes whether it accepts text. The P2P connection is lazy and
        /// persistent, not recreated for redraws or key events.
        pub fn set_ime_state(&mut self, id: u32, visible: bool) -> Result<(), String> {
            if !self.state.surfaces.contains_key(&id) {
                return Err(format!("IME request for unknown surface {id}"));
            }
            if !visible && self.state.maliit.is_none() {
                return Ok(());
            }
            if self.state.maliit.is_none() {
                self.state.maliit = Some(crate::ime::Maliit::connect()?);
            }
            let result = self.state.maliit.as_mut().unwrap().set_visible(visible);
            if result.is_ok() {
                self.state.ime_surface = visible.then_some(id);
            }
            if result.is_ok() {
                crate::info!("surface {id} IME visible={visible}");
            }
            result
        }

        /// Re-commit a surface so pixels the guest wrote into the shared
        /// buffer become visible. Pixel contents are owned by the guest; the
        /// daemon only drives the Wayland commit.
        pub fn present(&mut self, id: u32) -> bool {
            // Drain pending events first: the compositor may have destroyed the
            // surface (or asked to close it) since the guest's last frame, and a
            // request to a dead object is a protocol error.
            self.poll();
            if let Some(s) = self.state.surfaces.get_mut(&id) { s.loading = false; }
            let Some(s) = self.state.surfaces.get(&id) else { return false };
            if s.closed || s.width == 0 || s.height == 0 {
                return false;
            }
            s.wl_surface.attach(Some(&s.buffer), 0, 0);
            // A zero-area damage rect is a protocol violation.
            s.wl_surface.damage(0, 0, s.display_width as i32, s.display_height as i32);
            s.wl_surface.commit();
            // Flush errors mean the compositor is gone; report the frame as lost
            // rather than pretending it was shown.
            let presented = self.conn.flush().is_ok();
            if presented {
                self.record_frame(id);
                self.vsync_note_present(id);
            }
            presented
        }

        /// Create a child layer of `parent` at `(x, y)`.
        ///
        /// Android composites a `SurfaceView` as its own layer under the
        /// window; the Wayland equivalent is a `wl_subsurface`. It is created in
        /// desynchronised mode so the guest can commit frames without the
        /// parent having to commit too (a video frame must not wait for the UI).
        pub fn create_subsurface(
            &mut self,
            parent: u32,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
        ) -> Option<u32> {
            let subcompositor = self.state.subcompositor.clone()?;
            let compositor = self.state.compositor.clone()?;
            let shm = self.state.shm.clone()?;
            let parent_surface = self.state.surfaces.get(&parent)?.wl_surface.clone();
            let qh = self.queue.handle();

            let id = self.state.alloc_id();
            let wl_surface = compositor.create_surface(&qh, ());
            let subsurface = subcompositor.get_subsurface(&wl_surface, &parent_surface, &qh, ());
            // Android dispatches SurfaceView touch events through the parent
            // ViewRootImpl. This video-only child has no input subscription;
            // without an empty input region it swallows clicks over the video.
            let input_region = compositor.create_region(&qh, ());
            wl_surface.set_input_region(Some(&input_region));
            input_region.destroy();
            subsurface.set_position(x, y);
            subsurface.set_desync();

            // A subsurface still needs *a* buffer to be mapped; a 1x1
            // transparent one is attached until the first dma-buf arrives.
            let region = match AshmemRegion::create(&format!("pengudroid-sub-{id}"), 4) {
                Ok(r) => r,
                Err(e) => {
                    crate::warn!("cannot allocate subsurface memory: {e}");
                    subsurface.destroy();
                    wl_surface.destroy();
                    return None;
                }
            };
            let dup = region.try_clone_fd().ok()?;
            let pool = shm.create_pool(dup.as_fd(), region.size() as i32, &qh, ());
            let buffer = pool.create_buffer(
                0,
                1,
                1,
                4,
                wl_shm::Format::Argb8888,
                &qh,
                (),
            );
            pool.destroy();
            wl_surface.attach(Some(&buffer), 0, 0);
            wl_surface.damage(0, 0, 1, 1);
            wl_surface.commit();

            self.state.surfaces.insert(
                id,
                Surface {
                    wl_surface: wl_surface.clone(),
                    xdg_surface: None,
                    xdg_toplevel: None,
                    shell_surface: None,
                    subsurface: Some(subsurface),
                    parent_id: Some(parent),
                    buffer,
                    region,
                    dmabuf_fd: None,
                    pending_frames: Vec::new(),
                    width: width.max(1),
                    height: height.max(1),
                    configured: true,
                    closed: false,
                    loading: false,
                    fps_meter: false,
                    title: String::new(),
                    display_width: width.max(1),
                    display_height: height.max(1),
                    viewport: self.state.viewporter.as_ref().map(|v| v.get_viewport(&wl_surface, &qh, ())),
                    position: (x, y),
                    window_state: WindowState::empty(),
                    desktop_configured: true,
                },
            );
            self.state.scale_children(parent);
            self.state.surfaces.get(&parent)?.wl_surface.commit();
            let _ = self.conn.flush();
            crate::info!("created subsurface {id} of {parent} at {x},{y} size={width}x{height} (input transparent)");
            Some(id)
        }

        /// Move an existing subsurface inside its parent.
        pub fn set_subsurface_position(&mut self, id: u32, x: i32, y: i32) -> bool {
            let Some(s) = self.state.surfaces.get_mut(&id) else { return false };
            if s.subsurface.is_none() { return false; }
            s.position = (x, y);
            let Some(parent) = s.parent_id else { return false };
            self.state.scale_children(parent);
            self.state.surfaces.get(&parent).unwrap().wl_surface.commit();
            let _ = self.conn.flush();
            true
        }

        /// What a surface is showing: `(buffer_w, buffer_h, display_w, display_h)`.
        ///
        /// The first pair is the guest's own frame size, which a window resize
        /// deliberately leaves alone; the second is the size the compositor
        /// scales it into.
        pub fn surface_geometry(&mut self, id: u32) -> Option<(u32, u32, u32, u32)> {
            self.state.surfaces.get(&id).map(|s| {
                (s.width, s.height, s.display_width, s.display_height)
            })
        }

        /// Maximize, fullscreen, or return a window to normal. The compositor
        /// answers with a `configure`, which is what actually changes
        /// `display_width`/`display_height`; the guest buffer keeps its own
        /// size either way.
        pub fn set_window_state(&mut self, id: u32, state: u32) -> bool {
            let Some(top) = self.state.surfaces.get(&id).and_then(|s| s.xdg_toplevel.clone()) else {
                return false;
            };
            if state & pengudroid_proto::WINDOW_FULLSCREEN != 0 {
                // Fullscreen already fills the output; leaving maximized set
                // makes some compositors restore the maximized frame afterwards.
                top.unset_maximized();
                top.set_fullscreen(None);
            } else {
                top.unset_fullscreen();
                if state & pengudroid_proto::WINDOW_MAXIMIZED != 0 {
                    top.set_maximized();
                } else {
                    top.unset_maximized();
                }
            }
            let _ = self.conn.flush();
            true
        }

        /// The surface holding the pointer lock, if any.
        pub fn pointer_lock_surface(&self) -> Option<u32> {
            self.state.pointer_lock.as_ref().map(|(id, _)| *id)
        }

        /// Lock the pointer to surface `id`, or release the lock it holds.
        ///
        /// A lock is what turns a mouse into a camera control: the cursor stops
        /// at no window edge because it does not move at all, and motion comes
        /// from `zwp_relative_pointer_v1` instead. The lifetime is persistent,
        /// so the compositor re-activates it whenever the window regains
        /// pointer focus. Returns false when either protocol is missing.
        pub fn set_pointer_lock(&mut self, id: u32, locked: bool) -> bool {
            if !locked {
                // Releasing a lock that is not held (or not ours) is a no-op,
                // not an error: the guest is only stating what it wants.
                if self.state.pointer_lock.as_ref().is_some_and(|(owner, _)| *owner == id) {
                    self.state.release_pointer_lock();
                    let _ = self.conn.flush();
                }
                return true;
            }
            if self.state.pointer_lock.as_ref().is_some_and(|(owner, _)| *owner == id) {
                return true;
            }
            let qh = self.queue.handle();
            let (Some(constraints), Some(manager), Some(pointer)) = (
                self.state.pointer_constraints.clone(),
                self.state.relative_manager.clone(),
                self.state.pointer.clone(),
            ) else {
                return false;
            };
            let Some(surface) = self.state.surfaces.get(&id).map(|s| s.wl_surface.clone()) else {
                return false;
            };
            // One lock per pointer: a second request would be a protocol error.
            self.state.release_pointer_lock();
            let lock = constraints.lock_pointer(
                &surface,
                &pointer,
                None,
                zwp_pointer_constraints_v1::Lifetime::Persistent,
                &qh,
                (),
            );
            if self.state.relative_pointer.is_none() {
                self.state.relative_pointer = Some(manager.get_relative_pointer(&pointer, &qh, ()));
            }
            self.state.pointer_lock = Some((id, lock));
            let _ = self.conn.flush();
            crate::info!("pointer locked to surface {id}");
            true
        }

        /// Forward a loading-stage or error message to the placeholder screen.
        pub fn set_loading_status(&mut self, text: String) {
            self.state.set_loading_status(text);
        }

        pub fn set_surface_options(&mut self, id: u32, enabled: bool) -> bool {
            let Some(surface) = self.state.surfaces.get_mut(&id) else { return false };
            surface.fps_meter = enabled;
            let title = surface.title.clone();
            self.state.update_frame_title(id, title);
            let _ = self.conn.flush();
            true
        }

        fn record_frame(&mut self, id: u32) {
            let Some(surface) = self.state.surfaces.get(&id) else { return };
            let parent = surface.parent_id.unwrap_or(id);
            let Some(root) = self.state.surfaces.get(&parent) else { return };
            static LOG_FPS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let log_fps = *LOG_FPS.get_or_init(|| Self::env_enabled("PENGUDROID_FPS_LOG"));
            if !root.fps_meter && !log_fps { return; }
            // If video has a separate child layer, count decoder frames rather
            // than summing UI swaps with video frames.
            if id == parent && self.state.surfaces.values().any(|s| s.parent_id == Some(parent)) { return; }
            let now = std::time::Instant::now();
            let entry = self.state.fps.entry(id).or_insert((0, now));
            entry.0 += 1;
            let elapsed = now.duration_since(entry.1).as_secs_f32();
            if elapsed >= 1.0 {
                let fps = entry.0 as f32 / elapsed;
                entry.0 = 0; entry.1 = now;
                if log_fps {
                    // Count only frames whose present completed. Unlike the
                    // guest overlay, this adds no GL work to the measured path.
                    crate::info!("surface {id}: {fps:.1} FPS");
                }
            }
        }

        pub fn contains_surface(&self, id: u32) -> bool {
            self.state.surfaces.contains_key(&id)
        }

        pub fn is_toplevel(&self, id: u32) -> bool {
            // Lipstick uses wl_shell rather than xdg_toplevel.
            self.state.surfaces.get(&id)
                .is_some_and(|s| s.parent_id.is_none() && !s.closed)
        }

        /// Destroy a surface and its Wayland objects (the compositor closed the
        /// window, or the guest went away). Idempotent.
        pub fn destroy_surface(&mut self, id: u32) -> bool {
            // Retire children before their parent. Otherwise their commits are
            // still Acked even though Wayland can no longer display them.
            let children: Vec<u32> = self.state.surfaces.iter()
                .filter_map(|(child, s)| (s.parent_id == Some(id)).then_some(*child))
                .collect();
            for child in children {
                self.destroy_surface(child);
            }
            if self.state.keyboard_focus == Some(id) { self.state.release_keyboard(); }
            if self.state.pointer_focus == Some(id) { self.state.pointer_focus = None; }
            if self.state.pointer_lock.as_ref().is_some_and(|(owner, _)| *owner == id) {
                self.state.release_pointer_lock();
            }
            self.state.fps.remove(&id);
            self.state.frames.remove(&id);
            self.state.client_decorated.remove(&id);
            self.state.configured.remove(&id);
            self.state.wm_capabilities.remove(&id);
            if self.state.frame_focus.as_ref().is_some_and(|(owner, _)| *owner == id) { self.state.frame_focus = None; }
            if let Some(decoration) = self.state.decorations.remove(&id) { decoration.destroy(); }
            // Extended surface belongs to this wl_surface; drop it first.
            self.state.extended_surfaces.remove(&id);
            if let Some(maliit) = self.state.maliit.as_mut() {
                let _ = maliit.set_visible(false);
            }
            if self.state.ime_surface == Some(id) {
                self.state.ime_surface = None;
            }
            if let Some(presenter) = self.state.egl_presenter.as_mut() {
                // wl_egl_window must be destroyed before its wl_surface.
                presenter.destroy_surface(id);
            }
            let Some(surface) = self.state.surfaces.remove(&id) else { return false };
            if let Some(t) = surface.xdg_toplevel {
                t.destroy();
            }
            if let Some(x) = surface.xdg_surface {
                x.destroy();
            }
            if let Some(sub) = surface.subsurface {
                sub.destroy();
            }
            for frame in surface.pending_frames {
                // A leased decode surface cannot be recycled until the real
                // compositor release, even after its wl_surface is destroyed.
                let buffer = frame.buffer;
                let leased = buffer.data::<FrameBuffer>().is_some_and(|data| data.released.is_some());
                if buffer.is_alive() && !leased { buffer.destroy(); }
            }
            surface.buffer.destroy();
            if let Some(viewport) = surface.viewport { viewport.destroy(); }
            surface.wl_surface.destroy();
            let _ = self.conn.flush();
            crate::debug!("destroyed surface {id}");
            true
        }

        /// Process any queued events (non-blocking). Call periodically to learn
        /// about close requests.
        pub fn poll(&mut self) {
            self.drain_wayland();
        }

        pub fn close_requests(&mut self) -> Vec<u32> {
            self.poll();
            std::mem::take(&mut self.state.close_requests)
        }

        pub fn surface_close_requested(&self, id: u32) -> bool {
            self.state.surfaces.get(&id).map(|s| s.closed).unwrap_or(false)
        }

        /// Duplicate the backing memory fd for a surface so it can be sent to a
        /// guest (which mmaps it to render directly into the compositor's view).
        pub fn surface_fd(&self, id: u32) -> Option<std::os::fd::OwnedFd> {
            self.state.surfaces.get(&id).and_then(|s| s.region.try_clone_fd().ok())
        }

        /// Whether the compositor advertises `zwp_linux_dmabuf_v1` (v3+).
        pub fn dmabuf_available(&self) -> bool {
            self.state.dmabuf.is_some()
        }

        /// Number of dmabuf formats the compositor advertised.
        pub fn dmabuf_format_count(&self) -> u32 {
            self.state.dmabuf_supported.len() as u32
        }

        /// Whether the compositor advertised this exact `(fourcc, modifier)`.
        /// A modifier of 0 matches a plain `format` advertisement.
        pub fn dmabuf_supports(&self, fourcc: u32, modifier: u64) -> bool {
            self.state
                .dmabuf_supported
                .iter()
                .any(|(f, m)| *f == fourcc && (*m == modifier || *m == 0))
        }

        /// Process pending Wayland events (a blocking round-trip). Mostly useful
        /// to let the initial `format`/`modifier` burst from
        /// `zwp_linux_dmabuf_v1` be delivered before reading diagnostics.
        pub fn refresh(&mut self) {
            let _ = self.queue.roundtrip(&mut self.state);
        }

        /// Present a GPU buffer directly (no `wl_shm`, no CPU copy).
        ///
        /// Creates a throwaway toplevel, imports `fd` with
        /// `zwp_linux_dmabuf_v1` at the given geometry/format/modifier, attaches
        /// it, commits, and round-trips to learn whether the compositor accepted
        /// the buffer. This is the zero-copy counterpart of `create_toplevel`.
        #[allow(clippy::too_many_arguments)]
        pub fn dmabuf_selftest(
            &mut self,
            width: u32,
            height: u32,
            fd: std::os::fd::OwnedFd,
            stride: u32,
            fourcc: u32,
            modifier: u64,
            offset: u32,
        ) -> Result<(), String> {
            let compositor = self.state.compositor.clone().ok_or("no wl_compositor")?;
            let wm_base = self.state.wm_base.clone().ok_or("no xdg_wm_base")?;
            let dmabuf = self
                .state
                .dmabuf
                .clone()
                .ok_or("compositor lacks zwp_linux_dmabuf_v1 (v3+)")?;
            let qh = self.queue.handle();

            let wl_surface = compositor.create_surface(&qh, ());
            let xdg_surface = wm_base.get_xdg_surface(&wl_surface, &qh, ());
            // Throwaway toplevel for the dmabuf self-test: it owns no surface id.
            let toplevel = xdg_surface.get_toplevel(&qh, SurfaceId(u32::MAX));
            toplevel.set_title("PenguDroid dmabuf selftest".to_string());
            wl_surface.commit();

            self.state.dmabuf_created = false;
            self.state.dmabuf_failed = false;

            let params = dmabuf.create_params(&qh, ());
            params.add(
                fd.as_fd(),
                0,
                offset,
                stride,
                (modifier >> 32) as u32,
                (modifier & 0xffff_ffff) as u32,
            );
            let buffer = params.create_immed(
                width as i32,
                height as i32,
                fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &qh,
                (),
            );
            params.destroy();

            wl_surface.attach(Some(&buffer), 0, 0);
            wl_surface.damage(0, 0, width as i32, height as i32);
            wl_surface.commit();

            self.queue
                .roundtrip(&mut self.state)
                .map_err(|e| format!("roundtrip: {e}"))?;
            crate::debug!(
                "dmabuf selftest: created={} failed={}",
                self.state.dmabuf_created,
                self.state.dmabuf_failed
            );
            if self.state.dmabuf_failed {
                return Err("compositor rejected the dmabuf".to_string());
            }
            Ok(())
        }

        // Keep compositor backpressure bounded, including when a window is hidden.
        fn frame_slot_available(&mut self, id: u32) -> Result<bool, String> {
            self.drain_wayland();
            let surface = self.state.surfaces.get_mut(&id).ok_or_else(|| format!("unknown surface {id}"))?;
            surface.pending_frames.retain(|frame| frame.buffer.is_alive());
            Ok(surface.pending_frames.len() < 3)
        }

        fn profile_native_import(duration: std::time::Duration) {
            static PROFILE: std::sync::Mutex<(u32, Option<std::time::Instant>, std::time::Duration)> =
                std::sync::Mutex::new((0, None, std::time::Duration::ZERO));
            let mut profile = PROFILE.lock().unwrap_or_else(|e| e.into_inner());
            profile.0 += 1;
            profile.2 += duration;
            let elapsed = profile.1.map(|since| since.elapsed()).unwrap_or_default();
            if profile.1.is_none() || elapsed >= std::time::Duration::from_secs(1) {
                let count = profile.0.max(1) as f64;
                let fps = if elapsed.is_zero() { 0.0 } else { profile.0 as f64 / elapsed.as_secs_f64() };
                crate::info!(
                    "native wl_shm import: fps={fps:.1} pool+buffer+commit={:.2}ms",
                    profile.2.as_secs_f64() * 1000.0 / count,
                );
                profile.0 = 0;
                profile.2 = std::time::Duration::ZERO;
                profile.1 = Some(std::time::Instant::now());
            }
        }

        /// Import a full RGBA guest memfd without another CPU snapshot. The fd
        /// remains owned by PendingFrame until Lipstick releases the buffer.
        fn present_shm_direct(
            &mut self,
            id: u32,
            fd: std::os::fd::OwnedFd,
            width: u32,
            height: u32,
            stride: u32,
            native_gpu_buffer: bool,
        ) -> Result<(), String> {
            use std::os::fd::AsRawFd;
            let profile_started = std::time::Instant::now();
            let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
            if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
                return Err(format!("PresentShm fstat: {}", std::io::Error::last_os_error()));
            }
            let required = stride as i64 * height as i64;
            // dma-buf commonly reports size 0 through fstat. wl_shm still
            // needs an explicit mapping size, which is known from the layout.
            let pool_size = if stat.st_size == 0 { required } else { stat.st_size as i64 };
            if pool_size < required {
                return Err(format!("PresentShm: fd is {} bytes, need {required}", stat.st_size));
            }
            let shm = self.state.shm.as_ref().ok_or("no wl_shm")?;
            let qh = self.queue.handle();
            let pool = shm.create_pool(fd.as_fd(), pool_size as i32, &qh, ());
            let is_subsurface = self.state.surfaces.get(&id)
                .is_some_and(|surface| surface.subsurface.is_some());
            let buffer = pool.create_buffer(
                0, width as i32, height as i32, stride as i32,
                if is_subsurface { wl_shm::Format::Argb8888 } else { wl_shm::Format::Xrgb8888 },
                &qh, FrameBuffer::default(),
            );
            pool.destroy();
            let surface = self.state.surfaces.get_mut(&id).ok_or("surface disappeared")?;
            surface.loading = false;
            surface.wl_surface.set_buffer_transform(
                if native_gpu_buffer && surface.subsurface.is_none() {
                    wl_output::Transform::Flipped180
                } else {
                    wl_output::Transform::Normal
                },
            );
            surface.wl_surface.attach(Some(&buffer), 0, 0);
            surface.wl_surface.damage(0, 0, surface.display_width as i32, surface.display_height as i32);
            surface.wl_surface.commit();
            surface.pending_frames.push(PendingFrame { buffer, _fd: Some(fd), _native_fds: Vec::new() });
            self.conn.flush().map_err(|e| e.to_string())?;
            self.record_frame(id);
            self.vsync_note_present(id);
            if native_gpu_buffer {
                Self::profile_native_import(profile_started.elapsed());
            }
            Ok(())
        }

        /// Present a whole Android native handle through `android_wlegl`.
        ///
        /// The guest sends every fd of its gralloc `native_handle_t` plus the
        /// int words; the compositor imports that as a GPU texture, so no
        /// `wl_shm` pool and no CPU-visible read is involved.
        pub fn present_wlegl(
            &mut self,
            id: u32,
            fds: Vec<std::os::fd::OwnedFd>,
            ints: Vec<i32>,
            width: u32,
            height: u32,
            stride: u32,
            format: i32,
            usage: i32,
        ) -> Result<(), String> {
            if !self.state.wlegl_enabled {
                return Err("android_wlegl presentation is disabled".into());
            }
            if cfg!(target_arch = "arm") {
                // ARM32 default: hand the native buffer to Lipstick as a
                // wl_buffer directly. The Hybris Wayland EGL platform would
                // create a second event queue and dispatch it on this same
                // wl_display from eglCreateWindowSurface, racing the Rust
                // client's own dispatcher; on the tested ARM32/MediaTek
                // device that crashed the daemon on the first surface.
                return self.present_wlegl_buffer(id, fds, ints, width, height, stride, format, usage);
            }
            // AArch64: the daemon owns a real EGLSurface on the guest's
            // wl_surface and presents through eglSwapBuffers, so the Wayland
            // EGL driver handles the compositor fence (which the manual
            // android_wlegl wl_buffer path could not provide).
            if self.state.egl_presenter.is_none() {
                let display_ptr = self.conn.backend().display_ptr() as *mut std::ffi::c_void;
                match crate::egl::Presenter::new(display_ptr) {
                    Ok(presenter) => self.state.egl_presenter = Some(presenter),
                    Err(error) => return Err(format!("EGL presenter init failed: {error}")),
                }
            }
            let wl_surface = self.state.surfaces.get(&id)
                .ok_or("surface disappeared")?.wl_surface.clone();
            let presenter = self.state.egl_presenter.as_mut().unwrap();
            presenter.create_surface(id, &wl_surface, width, height)?;
            presenter.present(id, &fds, &ints, width, height, stride / 4, format, usage)?;
            // eglSwapBuffers issues attach/commit on this same connection.
            // Flush immediately; otherwise a new UI frame can sit queued until
            // an unrelated compositor event wakes the daemon.
            self.conn.flush().map_err(|e| format!("Wayland flush after EGL swap: {e}"))?;
            if let Some(surface) = self.state.surfaces.get_mut(&id) {
                surface.loading = false;
            }
            self.record_frame(id);
            self.vsync_note_present(id);
            static REPORTED: std::sync::Once = std::sync::Once::new();
            REPORTED.call_once(|| {
                crate::info!("guest frames presented through daemon EGL (eglSwapBuffers)");
            });
            Ok(())
        }

        /// Present a whole Android native handle as a `wl_buffer` through the
        /// compositor's `android_wlegl` (no second EGL dispatcher involved).
        /// This is the ARM32 default; the original transport from the opt-in
        /// commit, kept because it is the one path that never shares the
        /// `wl_display` with the Hybris Wayland EGL platform.
        fn present_wlegl_buffer(
            &mut self,
            id: u32,
            fds: Vec<std::os::fd::OwnedFd>,
            ints: Vec<i32>,
            width: u32,
            height: u32,
            stride: u32,
            format: i32,
            usage: i32,
        ) -> Result<(), String> {
            if !self.frame_slot_available(id)? { return Ok(()); }
            let wlegl = self.state.wlegl.clone()
                .ok_or("android_wlegl is not advertised by the compositor")?;
            // One native handle -> one wl_buffer for the whole surface's life.
            // The int words identify the allocation even though the fds are
            // duplicated on every present.
            let key = (id, fnv1a_ints(&ints));
            let buffer = if let Some((buffer, _fds)) = self.state.wlegl_buffers.get(&key) {
                buffer.clone()
            } else {
                crate::info!(
                    "wlegl new wl_buffer: surface={id} key={:#x} fds={} total={}",
                    key.1,
                    fds.len(),
                    self.state.wlegl_buffers.len() + 1
                );
                let qh = self.queue.handle();
                let handle = wlegl.create_handle(fds.len() as i32, ints_as_bytes(&ints), &qh, ());
                for fd in &fds {
                    handle.add_fd(fd.as_fd());
                }
                // stride is in pixels for Android native buffers, not bytes.
                let buffer = wlegl.create_buffer(
                    width as i32, height as i32, (stride / 4) as i32, format, usage, &handle, &qh, (),
                );
                handle.destroy();
                self.state.wlegl_buffers.insert(key, (buffer.clone(), fds));
                buffer
            };
            let surface = self.state.surfaces.get_mut(&id).ok_or("surface disappeared")?;
            surface.loading = false;
            // Lipstick ignores wl_surface buffer transforms; the guest already
            // flipped this buffer on the GPU before presenting it.
            surface.wl_surface.set_buffer_transform(wl_output::Transform::Normal);
            surface.wl_surface.attach(Some(&buffer), 0, 0);
            surface.wl_surface.damage(0, 0, surface.display_width as i32, surface.display_height as i32);
            surface.wl_surface.commit();
            surface.pending_frames.push(PendingFrame {
                buffer,
                _fd: None,
                _native_fds: Vec::new(),
            });
            self.conn.flush().map_err(|e| e.to_string())?;
            self.record_frame(id);
            self.vsync_note_present(id);
            static REPORTED: std::sync::Once = std::sync::Once::new();
            REPORTED.call_once(|| {
                crate::info!("guest frames presented through android_wlegl (GPU native buffers)");
            });
            Ok(())
        }

        /// Snapshot decoder-owned CPU memory and submit it through wl_shm.
        /// Every frame has separate storage until the compositor releases it.
        pub fn present_shm(&mut self, id: u32, fd: std::os::fd::OwnedFd,
            width: u32, height: u32, stride: u32, format: u32, crop: BufferCrop) -> Result<(), String> {
            let source_size = crop.dimensions(width, height)?;
            if !self.frame_slot_available(id)? { return Ok(()); }
            let native_gpu_buffer = format & abi::NATIVE_FD_FLAG != 0;
            let direct = format & !abi::NATIVE_FD_FLAG == abi::ANDROID_RGBA_8888
                && crop == BufferCrop::full(width as i32, height as i32)
                && stride >= width * 4
                && self.state.surfaces.get(&id).is_some_and(|surface|
                    surface.subsurface.is_none()
                        || surface.viewport.is_some()
                        || (width, height) == (surface.width, surface.height));
            static LOGGED_SHM_INPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !LOGGED_SHM_INPUT.swap(true, std::sync::atomic::Ordering::Relaxed) {
                crate::info!(
                    "SHM input {width}x{height} stride={stride} format={format:#x} crop={crop:?} direct={direct}"
                );
            }
            if direct {
                return self.present_shm_direct(id, fd, width, height, stride, native_gpu_buffer);
            }
            let pixels = crate::software::snapshot(fd, width, height, stride, format, crop)?;
            let surface = self.state.surfaces.get(&id).unwrap();
            // wp_viewporter scales on the compositor/GPU. Only the legacy
            // path without a viewport needs a CPU-resized snapshot.
            let (width, height) = if surface.subsurface.is_some() && surface.viewport.is_none() {
                (surface.width, surface.height)
            } else { source_size };
            let pixels = crate::software::resize(pixels, source_size.0, source_size.1, width, height)?;
            if let Ok(dir) = std::env::var("PENGUDROID_DUMP_PRESENT") {
                // Diagnostic: proves what the compositor is about to attach,
                // including a YUV video frame after the CPU conversion.
                let path = std::path::Path::new(&dir)
                    .join(format!("present-{id}-{width}x{height}.ppm"));
                if std::fs::metadata(&path).is_err() {
                    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
                    for pixel in pixels.chunks_exact(4) {
                        out.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                    }
                    let _ = std::fs::write(&path, out);
                }
            }
            let region = AshmemRegion::create("pengudroid-video-snapshot", pixels.len() as u64)
                .map_err(|e| e.to_string())?;
            region.map_mut().map_err(|e| e.to_string())?.as_mut_slice().copy_from_slice(&pixels);
            let shm = self.state.shm.as_ref().ok_or("no wl_shm")?;
            let qh = self.queue.handle();
            let duplicate = region.try_clone_fd().map_err(|e| e.to_string())?;
            let pool = shm.create_pool(duplicate.as_fd(), pixels.len() as i32, &qh, ());
            let buffer = pool.create_buffer(0, width as i32, height as i32,
                (width * 4) as i32, wl_shm::Format::Xrgb8888, &qh, FrameBuffer::default());
            pool.destroy();
            let s = self.state.surfaces.get_mut(&id).unwrap();
            s.loading = false;
            s.wl_surface.set_buffer_transform(wl_output::Transform::Normal);
            s.wl_surface.attach(Some(&buffer), 0, 0);
            s.wl_surface.damage(0, 0, s.display_width as i32, s.display_height as i32);
            s.wl_surface.commit();
            s.pending_frames.push(PendingFrame { buffer, _fd: None, _native_fds: Vec::new() });
            // The pool request sends its own fd. Wayland retains the snapshot
            // independently of the Android buffer, which may now be recycled.
            self.conn.flush().map_err(|e| e.to_string())?;
            self.record_frame(id);
            Ok(())
        }

        /// Import a synchronized, top-down multi-plane video frame without CPU
        /// conversion. Keep the source HardwareFrame until the returned flag is
        /// true (wl_buffer.release). None means dropped before import.
        pub fn present_video_dmabuf(&mut self, id: u32, frame: pengudroid_hostgpu::video::ExportedFrame)
            -> Result<Option<Arc<std::sync::atomic::AtomicBool>>, String> {
            frame.validate()?;
            if !self.frame_slot_available(id)? { return Ok(None); }
            for plane in &frame.planes {
                if !self.state.dmabuf_supported.contains(&(frame.fourcc, plane.modifier)) {
                    return Err(format!("compositor did not advertise video format {:#x} modifier {:#x}", frame.fourcc, plane.modifier));
                }
            }
            let dmabuf = self.state.dmabuf.as_ref().ok_or("compositor lacks linux-dmabuf")?;
            let qh = self.queue.handle();
            let params = dmabuf.create_params(&qh, ());
            for (index, plane) in frame.planes.iter().enumerate() {
                params.add(frame.objects[plane.object_index as usize].as_fd(), index as u32,
                    plane.offset, plane.stride, (plane.modifier >> 32) as u32, plane.modifier as u32);
            }
            let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let buffer = params.create_immed(frame.width as i32, frame.height as i32, frame.fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(), &qh, FrameBuffer { released: Some(released.clone()) });
            params.destroy();
            let surface = self.state.surfaces.get_mut(&id).ok_or("video surface disappeared")?;
            surface.wl_surface.set_buffer_transform(wl_output::Transform::Normal);
            surface.wl_surface.attach(Some(&buffer), 0, 0);
            surface.wl_surface.damage(0, 0, surface.display_width as i32, surface.display_height as i32);
            surface.wl_surface.commit();
            surface.pending_frames.push(PendingFrame { buffer, _fd: None, _native_fds: Vec::new() });
            self.conn.flush().map_err(|e| e.to_string())?;
            self.record_frame(id);
            Ok(Some(released))
        }

        /// Present a GPU-rendered dma-buf on an existing surface (zero-copy).
        ///
        /// This is what a guest's `eglSwapBuffers` ultimately becomes: a buffer
        /// the GPU wrote, imported by the compositor without a CPU copy.
        #[allow(clippy::too_many_arguments)]
        pub fn present_dmabuf(
            &mut self,
            id: u32,
            fd: std::os::fd::OwnedFd,
            width: u32,
            height: u32,
            stride: u32,
            fourcc: u32,
            modifier: u64,
            offset: u32,
            top_down: bool,
        ) -> Result<(), String> {
            if !self.frame_slot_available(id)? { return Ok(()); }
            let advertised = self.state.dmabuf_supported.contains(&(fourcc, modifier));
            if !advertised {
                crate::debug!(
                    "dma-buf {fourcc:#x}/{modifier:#x} was not in the compositor list ({} events); trying import anyway",
                    self.state.dmabuf_formats,
                );
            }
            let dmabuf = self
                .state
                .dmabuf
                .clone()
                .ok_or("compositor lacks zwp_linux_dmabuf_v1")?;
            let qh = self.queue.handle();
            let Some(s) = self.state.surfaces.get_mut(&id) else {
                return Err(format!("no such surface {id}"));
            };
            static LOGGED_DMABUF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !LOGGED_DMABUF.swap(true, std::sync::atomic::Ordering::Relaxed) {
                crate::info!(
                    "first dma-buf present surface={id} {width}x{height} stride={stride} fourcc={fourcc:#x} modifier={modifier:#x}"
                );
            }
            s.loading = false;
            let params = dmabuf.create_params(&qh, ());
            params.add(
                fd.as_fd(),
                0,
                offset,
                stride,
                (modifier >> 32) as u32,
                (modifier & 0xffff_ffff) as u32,
            );
            let buffer = params.create_immed(
                width as i32,
                height as i32,
                fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &qh,
                FrameBuffer::default(),
            );
            params.destroy();

            // The guest renders into a GL framebuffer, whose first row in memory is
            // the *bottom* of the image; `zwp_linux_dmabuf_v1` buffers are defined
            // top row first, like `wl_shm`. Without this the window comes out
            // upside down (the app bar along the bottom, text rotated 180°).
            //
            // `flipped_180` is "flip about the vertical axis, then rotate 180°",
            // which composes to a flip about the horizontal axis — a vertical flip,
            // exactly what is needed here. It is free: the compositor applies it
            // while sampling, so nothing is copied.
            //
            // `PENGUDROID_NO_PRESENT_FLIP=1` turns it off, for a compositor that
            // ignores buffer transforms or a guest that already renders top-down.
            //
            // A subsurface is fed by a decoder writing rows top-down, so it must
            // not be flipped.
            if s.subsurface.is_none() {
                let transform = if !top_down && std::env::var_os("PENGUDROID_NO_PRESENT_FLIP").is_none() {
                    wl_output::Transform::Flipped180
                } else {
                    wl_output::Transform::Normal
                };
                s.wl_surface.set_buffer_transform(transform);
            }
            s.wl_surface.attach(Some(&buffer), 0, 0);
            s.wl_surface.damage(0, 0, s.display_width as i32, s.display_height as i32);
            s.wl_surface.commit();
            s.pending_frames.push(PendingFrame { buffer, _fd: None, _native_fds: Vec::new() });
            s.dmabuf_fd = Some(fd);
            let _ = self.conn.flush();

            self.record_frame(id);
            Ok(())
        }
    }

    impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
        fn event(
            state: &mut Self,
            _proxy: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
            event: zwp_linux_dmabuf_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                zwp_linux_dmabuf_v1::Event::Format { format } => {
                    state.dmabuf_formats = state.dmabuf_formats.saturating_add(1);
                    if !state.dmabuf_supported.iter().any(|(f, _)| *f == format) {
                        state.dmabuf_supported.push((format, 0));
                    }
                }
                zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } => {
                    state.dmabuf_formats = state.dmabuf_formats.saturating_add(1);
                    let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
                    if !state.dmabuf_supported.contains(&(format, modifier)) {
                        state.dmabuf_supported.push((format, modifier));
                    }
                }
                _ => {}
            }
        }
    }

    impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for State {
        fn event(
            state: &mut Self,
            _proxy: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
            event: zwp_linux_buffer_params_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                zwp_linux_buffer_params_v1::Event::Created { .. } => state.dmabuf_created = true,
                zwp_linux_buffer_params_v1::Event::Failed => state.dmabuf_failed = true,
                _ => {}
            }
        }
    }

    // --- Dispatch implementations -------------------------------------------

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event,
            _: &GlobalListContents, _: &Connection, qh: &QueueHandle<Self>) {
            match event {
                wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                    if !state.output_proxies.contains_key(&name) {
                        let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, name);
                        let logical = state.output_manager.as_ref().map(|manager| manager.get_xdg_output(&output, qh, name));
                        state.outputs.insert(name, OutputSize::default());
                        state.output_proxies.insert(name, (output, logical));
                    }
                }
                wl_registry::Event::GlobalRemove { name } => {
                    state.outputs.remove(&name);
                    if let Some((output, logical)) = state.output_proxies.remove(&name) {
                        if let Some(logical) = logical { logical.destroy(); }
                        if output.version() >= 3 { output.release(); }
                    }
                }
                _ => {}
            }
        }
    }

    macro_rules! ignore_events {
        ($iface:ty, $udata:ty) => {
            impl Dispatch<$iface, $udata> for State {
                fn event(
                    _: &mut Self,
                    _: &$iface,
                    _: <$iface as wayland_client::Proxy>::Event,
                    _: &$udata,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        };
    }

    /// Per-frame imports are retired only after wl_buffer.release.
    #[derive(Default)]
    struct FrameBuffer { released: Option<Arc<std::sync::atomic::AtomicBool>> }
    impl Dispatch<wl_buffer::WlBuffer, FrameBuffer> for State {
        fn event(_: &mut Self, buffer: &wl_buffer::WlBuffer, event: wl_buffer::Event,
            data: &FrameBuffer, _: &Connection, _: &QueueHandle<Self>) {
            if let wl_buffer::Event::Release = event {
                if let Some(released) = &data.released { released.store(true, std::sync::atomic::Ordering::Release); }
                buffer.destroy();
            }
        }
    }

    ignore_events!(zxdg_output_manager_v1::ZxdgOutputManagerV1, ());
    impl Dispatch<wl_output::WlOutput, u32> for State {
        fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event,
            id: &u32, _: &Connection, _: &QueueHandle<Self>) {
            let output = state.outputs.entry(*id).or_default();
            match event {
                wl_output::Event::Mode { flags, width, height, .. } => {
                    if matches!(flags, wayland_client::WEnum::Value(flags) if flags.contains(wl_output::Mode::Current)) {
                        output.pixels = (width.max(0) as u32, height.max(0) as u32);
                    }
                }
                wl_output::Event::Scale { factor } => output.scale = factor.max(1) as u32,
                wl_output::Event::Geometry { transform, .. } => {
                    output.rotated = matches!(transform, wayland_client::WEnum::Value(
                        wl_output::Transform::_90 | wl_output::Transform::_270 |
                        wl_output::Transform::Flipped90 | wl_output::Transform::Flipped270));
                }
                _ => {}
            }
        }
    }
    impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
        fn event(state: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, event: zxdg_output_v1::Event,
            id: &u32, _: &Connection, _: &QueueHandle<Self>) {
            if let zxdg_output_v1::Event::LogicalSize { width, height } = event {
                if width > 0 && height > 0 { state.outputs.entry(*id).or_default().logical = Some((width as u32, height as u32)); }
            }
        }
    }

    ignore_events!(wp_viewporter::WpViewporter, ());
    ignore_events!(wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, ());
    ignore_events!(wp_cursor_shape_device_v1::WpCursorShapeDeviceV1, ());
    ignore_events!(wp_viewport::WpViewport, ());
    ignore_events!(wl_compositor::WlCompositor, smithay_client_toolkit::globals::GlobalData);
    smithay_client_toolkit::delegate_subcompositor!(State);
    impl Dispatch<wl_surface::WlSurface, SurfaceData> for State {
        fn event(state: &mut Self, surface: &wl_surface::WlSurface, event: wl_surface::Event,
            _: &SurfaceData, _: &Connection, _: &QueueHandle<Self>) {
            if let wl_surface::Event::PreferredBufferScale { factor } = event {
                if let Some(id) = state.surface_id_of(surface) {
                    if let Some(frame) = state.frames.get_mut(&id) { frame.set_scaling_factor(factor as f64); }
                    state.redraw_frame(id);
                }
            }
        }
    }

    ignore_events!(wl_compositor::WlCompositor, ());
    ignore_events!(wl_shm::WlShm, ());
    ignore_events!(wl_shm_pool::WlShmPool, ());
    ignore_events!(wl_buffer::WlBuffer, ());
    ignore_events!(wl_surface::WlSurface, ());
    ignore_events!(wl_region::WlRegion, ());
    // The compositor's frame callback is the guest's display clock.
    impl Dispatch<wl_callback::WlCallback, ()> for State {
        fn event(state: &mut Self, callback: &wl_callback::WlCallback,
            event: <wl_callback::WlCallback as wayland_client::Proxy>::Event,
            _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let wl_callback::Event::Done { callback_data } = event {
            let id = state.vsync.iter()
                .find_map(|(id, entry)| {
                    entry.callback.as_ref().is_some_and(|c| c == callback).then_some(*id)
                });
            if let Some(id) = id {
                state_on_vsync(state, id, callback_data);
            }
            }
        }
    }

    fn state_on_vsync(state: &mut State, id: u32, callback_ns: u32) {
        state.on_vsync_done(id, callback_ns);
    }

    ignore_events!(AndroidWlegl, ());
    ignore_events!(AndroidWleglHandle, ());
    ignore_events!(wl_subcompositor::WlSubcompositor, ());
    ignore_events!(wl_subsurface::WlSubsurface, ());

    impl Dispatch<wl_seat::WlSeat, ()> for State {
        fn event(
            state: &mut Self,
            seat: &wl_seat::WlSeat,
            event: wl_seat::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_seat::Event::Capabilities { capabilities } = event {
                use wayland_client::protocol::wl_seat::Capability;
                use wayland_client::WEnum;
                let has = |c: Capability| matches!(capabilities, WEnum::Value(v) if v.contains(c));
                if has(Capability::Pointer) && state.pointer.is_none() {
                    let pointer = seat.get_pointer(qh, ());
                    state.cursor = state.cursor_manager.as_ref().map(|manager| manager.get_pointer(&pointer, qh, ()));
                    state.pointer = Some(pointer);
                    crate::info!("seat: pointer available");
                }
                if has(Capability::Touch) && state.touch.is_none() {
                    state.touch = Some(seat.get_touch(qh, ()));
                    crate::info!("seat: touch available");
                }
                if has(Capability::Keyboard) && state.keyboard.is_none() {
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                    crate::info!("seat: keyboard available");
                }
            }
        }
    }

    ignore_events!(zwp_pointer_constraints_v1::ZwpPointerConstraintsV1, ());
    ignore_events!(zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1, ());
    // `locked`/`unlocked` only report activation; with a persistent lifetime
    // the compositor re-locks on its own, so there is nothing to do for them.
    ignore_events!(zwp_locked_pointer_v1::ZwpLockedPointerV1, ());

    impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for State {
        fn event(
            state: &mut Self,
            _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
            event: zwp_relative_pointer_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // The relative pointer reports motion whether or not a lock is
            // active; only a locked surface asked for it.
            if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
                if let Some((id, _)) = &state.pointer_lock {
                    state.input.push((
                        *id,
                        pengudroid_proto::InputKind::PointerRelative { dx: dx as f32, dy: dy as f32 },
                    ));
                }
            }
        }
    }

    ignore_events!(wl_data_device_manager::WlDataDeviceManager, ());

    impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
        fn event(
            state: &mut Self,
            _: &wl_data_device::WlDataDevice,
            event: wl_data_device::Event,
            _: &(), _: &Connection, _: &QueueHandle<Self>,
        ) {
            match event {
                wl_data_device::Event::DataOffer { id } => {
                    state.clipboard_offers.insert(id.id(), ClipboardOffer {
                        proxy: id,
                        mime_types: Vec::new(),
                    });
                }
                wl_data_device::Event::Selection { id } => {
                    state.clipboard_offer = id.map(|offer| offer.id());
                    state.clipboard_read_cache = None;
                }
                _ => {}
            }
        }

        wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [
            wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ())
        ]);
    }

    impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
        fn event(
            state: &mut Self,
            proxy: &wl_data_offer::WlDataOffer,
            event: wl_data_offer::Event,
            _: &(), _: &Connection, _: &QueueHandle<Self>,
        ) {
            if let wl_data_offer::Event::Offer { mime_type } = event {
                if let Some(offer) = state.clipboard_offers.get_mut(&proxy.id()) {
                    offer.mime_types.push(mime_type);
                }
            }
        }
    }

    impl Dispatch<wl_data_source::WlDataSource, ()> for State {
        fn event(
            state: &mut Self,
            proxy: &wl_data_source::WlDataSource,
            event: wl_data_source::Event,
            _: &(), _: &Connection, _: &QueueHandle<Self>,
        ) {
            match event {
                wl_data_source::Event::Send { mime_type: _, fd } => {
                    use std::os::fd::AsRawFd;
                    let Some(source) = state.clipboard_sources.get(&proxy.id()) else { return };
                    let bytes = source.text.as_bytes();
                    let started = std::time::Instant::now();
                    let mut written = 0usize;
                    while written < bytes.len() {
                        let result = unsafe {
                            libc::write(fd.as_raw_fd(), bytes[written..].as_ptr() as *const c_void,
                                bytes.len() - written)
                        };
                        if result > 0 { written += result as usize; continue; }
                        if result == 0 { break; }
                        let error = std::io::Error::last_os_error();
                        if error.kind() != std::io::ErrorKind::WouldBlock { break; }
                        if started.elapsed() >= std::time::Duration::from_millis(500) { break; }
                        let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                        unsafe { libc::poll(&mut poll, 1, 20) };
                    }
                    crate::debug!("clipboard source sent {} bytes", written);
                }
                wl_data_source::Event::Cancelled => {
                    if let Some(source) = state.clipboard_sources.remove(&proxy.id()) {
                        source.proxy.destroy();
                    }
                }
                _ => {}
            }
        }
    }

    ignore_events!(qt_surface_extension::QtSurfaceExtension, ());

    impl Dispatch<qt_extended_surface::QtExtendedSurface, SurfaceId> for State {
        fn event(
            state: &mut Self,
            _: &qt_extended_surface::QtExtendedSurface,
            event: qt_extended_surface::Event,
            id: &SurfaceId,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                qt_extended_surface::Event::Close => {
                    crate::info!("qt_extended_surface close for surface {}", id.0);
                    state.request_close(id.0);
                }
                qt_extended_surface::Event::OnscreenVisibility { visible } => {
                    crate::info!(
                        "surface {} onscreen visibility={}", id.0, visible != 0,
                    );
                    state.input.push((
                        id.0,
                        pengudroid_proto::InputKind::WindowVisibility { visible: visible != 0 },
                    ));
                    if visible != 0 {
                        if let Some(extended) = state.extended_surfaces.get(&id.0) {
                            extended.update_generic_property(
                                "STATUSBAR_VISIBLE".to_string(),
                                crate::qt_extended_surface::qvariant_bool(true),
                            );
                        }
                    } else {
                        state.hide_ime_surface(id.0, "offscreen");
                    }
                }
                qt_extended_surface::Event::SetGenericProperty { name, value } => {
                    crate::debug!(
                        "surface {} generic property {} ({} bytes)",
                        id.0, name, value.len(),
                    );
                }
            }
        }
    }

    impl Dispatch<wl_touch::WlTouch, ()> for State {
        fn event(
            state: &mut Self,
            _: &wl_touch::WlTouch,
            event: wl_touch::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            use pengudroid_proto::InputKind;
            match event {
                wl_touch::Event::Down { surface, x, y, id, serial, .. } => {
                    state.clipboard_selection_serial = Some(serial);
                    let Some(surface_id) = state.surface_id_of(&surface) else { return };
                    crate::info!("touch down id={id} surface={surface_id} at {x:.0},{y:.0}");
                    state.touch_points.insert(id, surface_id);
                    state.pointer_focus = Some(surface_id);
                    state.pointer_pos = (x, y);
                    let (gx, gy) = state.guest_coordinates(surface_id, x, y);
                    state.push_input(InputKind::PointerEnter { x: gx, y: gy });
                    state.push_input(InputKind::PointerMotion { x: gx, y: gy });
                    state.push_input(InputKind::PointerButton { button: 0x110, pressed: true });
                }
                wl_touch::Event::Motion { x, y, id, .. } => {
                    let Some(surface_id) = state.touch_points.get(&id).copied() else { return };
                    state.pointer_focus = Some(surface_id);
                    state.pointer_pos = (x, y);
                    let (gx, gy) = state.guest_coordinates(surface_id, x, y);
                    state.push_input(InputKind::PointerMotion { x: gx, y: gy });
                }
                wl_touch::Event::Up { id, .. } => {
                    let Some(surface_id) = state.touch_points.remove(&id) else { return };
                    crate::info!("touch up id={id} surface={surface_id}");
                    state.pointer_focus = Some(surface_id);
                    let (x, y) = state.pointer_pos;
                    let (gx, gy) = state.guest_coordinates(surface_id, x, y);
                    state.push_input(InputKind::PointerMotion { x: gx, y: gy });
                    state.push_input(InputKind::PointerButton { button: 0x110, pressed: false });
                    if state.touch_points.is_empty() {
                        state.push_input(InputKind::PointerLeave);
                        state.pointer_focus = None;
                    }
                }
                wl_touch::Event::Cancel { .. } => {
                    let ids: Vec<i32> = state.touch_points.keys().copied().collect();
                    for id in ids {
                        if let Some(surface_id) = state.touch_points.remove(&id) {
                            state.pointer_focus = Some(surface_id);
                            state.push_input(InputKind::PointerButton { button: 0x110, pressed: false });
                        }
                    }
                    state.push_input(InputKind::PointerLeave);
                    state.pointer_focus = None;
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wl_pointer::WlPointer, ()> for State {
        fn event(
            state: &mut Self,
            _: &wl_pointer::WlPointer,
            event: wl_pointer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            use pengudroid_proto::InputKind;
            match event {
                wl_pointer::Event::Enter { surface, surface_x, surface_y, serial } => {
                    state.pointer_serial = serial;
                    state.set_cursor(CursorIcon::Default);
                    state.frame_focus = None;
                    if state.frame_enter(&surface, surface_x, surface_y) {
                        state.pointer_focus = None;
                        return;
                    }
                    state.pointer_focus = state.surface_id_of(&surface);
                    state.pointer_pos = (surface_x, surface_y);
                    crate::info!(
                        "pointer entered surface {:?} at {surface_x:.0},{surface_y:.0}",
                        state.pointer_focus
                    );
                    let (x, y) = state.pointer_focus.map(|id| state.guest_coordinates(id, surface_x, surface_y))
                        .unwrap_or((surface_x as f32, surface_y as f32));
                    state.push_input(InputKind::PointerEnter { x, y });
                }
                wl_pointer::Event::Leave { .. } => {
                    if let Some((id, _)) = state.frame_focus.take() {
                        if let Some(frame) = state.frames.get_mut(&id) { frame.click_point_left(); }
                        state.redraw_frame(id);
                    }
                    state.push_input(InputKind::PointerLeave);
                    state.pointer_focus = None;
                }
                wl_pointer::Event::Motion { surface_x, surface_y, time } => {
                    if state.frame_motion(time, surface_x, surface_y) { return; }
                    state.pointer_pos = (surface_x, surface_y);
                    let (x, y) = state.pointer_focus.map(|id| state.guest_coordinates(id, surface_x, surface_y))
                        .unwrap_or((surface_x as f32, surface_y as f32));
                    state.push_input(InputKind::PointerMotion { x, y });
                }
                wl_pointer::Event::Button { button, state: button_state, time, serial } => {
                    use wayland_client::protocol::wl_pointer::ButtonState;
                    use wayland_client::WEnum;
                    let pressed = matches!(button_state, WEnum::Value(ButtonState::Pressed));
                    if pressed { state.clipboard_selection_serial = Some(serial); }
                    if state.frame_button(button, pressed, time, serial) { return; }
                    state.push_input(InputKind::PointerButton { button, pressed });
                }
                wl_pointer::Event::Axis { axis, value, .. } => {
                    use wayland_client::protocol::wl_pointer::Axis;
                    use wayland_client::WEnum;
                    state.push_input(InputKind::PointerAxis {
                        axis: if matches!(axis, WEnum::Value(Axis::VerticalScroll)) { 0 } else { 1 },
                        value: value as f32,
                    });
                }
                _ => {}
            }
        }
    }

    /// Map the keymap fd the compositor sent and copy out its NUL-terminated text.
    fn read_keymap(fd: std::os::fd::OwnedFd, size: usize) -> Option<std::ffi::CString> {
        use std::os::fd::AsRawFd;
        if size == 0 {
            return None;
        }
        // SAFETY: a private read-only mapping of the compositor's keymap fd.
        unsafe {
            let map = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_PRIVATE, fd.as_raw_fd(), 0);
            if map == libc::MAP_FAILED {
                return None;
            }
            let bytes = std::slice::from_raw_parts(map as *const u8, size);
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(size);
            let text = std::ffi::CString::new(&bytes[..end]).ok();
            libc::munmap(map, size);
            text
        }
    }

    impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
        fn event(
            state: &mut Self,
            _: &wl_keyboard::WlKeyboard,
            event: wl_keyboard::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            use pengudroid_proto::InputKind;
            use wayland_client::protocol::wl_keyboard::KeyState;
            use wayland_client::WEnum;
            match event {
                wl_keyboard::Event::Enter { surface, keys, .. } => {
                    state.release_keyboard();
                    state.keyboard_focus = state.surface_id_of(&surface);
                    // Compositor-provided held modifiers are physical keycodes,
                    // not characters. Restore them on this keyboard focus only.
                    if let Some(id) = state.keyboard_focus {
                        for key in keys.chunks_exact(4) {
                            let keycode = u32::from_ne_bytes(key.try_into().unwrap());
                            state.held_keys.insert(keycode);
                            state.input.push((id, InputKind::Key { keycode, pressed: true }));
                        }
                    }
                }
                wl_keyboard::Event::Leave { surface, .. } => {
                    // Same lifecycle boundary as Aurora Embedder's
                    // View::SeatClient::KeyboardLeave(): a window losing seat
                    // focus must not leave Maliit or its touch filter active.
                    let id = state.surface_id_of(&surface);
                    state.release_keyboard();
                    if let Some(id) = id { state.hide_ime_surface(id, "keyboard leave"); }
                }
                wl_keyboard::Event::Key { key, state: key_state, serial, .. } => {
                    let pressed = matches!(key_state, WEnum::Value(KeyState::Pressed));
                    if pressed { state.clipboard_selection_serial = Some(serial); }
                    state.keyboard_key(key, pressed);
                }
                wl_keyboard::Event::Keymap { format, fd, size } => {
                    use wayland_client::protocol::wl_keyboard::KeymapFormat;
                    // Guests build their KeyCharacterMap from this file, so a
                    // key types the user's layout (Turkish ı, AltGr symbols).
                    let path = pengudroid_proto::keymap_path();
                    let written = matches!(format, WEnum::Value(KeymapFormat::XkbV1))
                        && read_keymap(fd, size as usize).is_some_and(|text| {
                            let temp = path.with_extension("keymap.tmp");
                            std::fs::write(&temp, text.as_bytes()).is_ok() && std::fs::rename(&temp, &path).is_ok()
                        });
                    if written {
                        crate::info!("keyboard layout published at {}", path.display());
                    } else {
                        let _ = std::fs::remove_file(&path);
                        crate::info!("keyboard layout unavailable; guests use a US key map");
                    }
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wl_shell::WlShell, ()> for State {
        fn event(
            _: &mut Self,
            _: &wl_shell::WlShell,
            _: wl_shell::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {}
    }

    impl Dispatch<wl_shell_surface::WlShellSurface, SurfaceId> for State {
        fn event(
            state: &mut Self,
            proxy: &wl_shell_surface::WlShellSurface,
            event: wl_shell_surface::Event,
            _: &SurfaceId,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let Some(id) = proxy.data::<SurfaceId>().map(|data| data.0) else { return };
            match event {
                wl_shell_surface::Event::Ping { serial } => proxy.pong(serial),
                wl_shell_surface::Event::Configure { width, height, .. } => {
                    state.configured.insert(id, (width, height, WindowState::empty()));
                }
                _ => {}
            }
        }
    }

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
        fn event(
            _: &mut Self,
            proxy: &xdg_wm_base::XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // Compositor liveness ping: must be answered or it drops us.
            if let xdg_wm_base::Event::Ping { serial } = event {
                proxy.pong(serial);
            }
        }
    }

    impl Dispatch<xdg_surface::XdgSurface, ()> for State {
        fn event(
            state: &mut Self,
            proxy: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                proxy.ack_configure(serial);
                if let Some(id) = state.pending.iter().find_map(|p| {
                    (p.xdg_surface.as_ref() == Some(proxy)).then_some(p.id)
                }) {
                    state.finalize_surface(id, qh);
                }
                if let Some(id) = state.surfaces.iter().find_map(|(id, s)|
                    (s.xdg_surface.as_ref() == Some(proxy)).then_some(*id)) {
                    state.configure_window(id, qh);
                }
            }
        }
    }

    impl Dispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
            _: zxdg_decoration_manager_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // The manager has no events.
        }
    }

    impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, SurfaceId> for State {
        fn event(state: &mut Self, _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
            event: zxdg_toplevel_decoration_v1::Event, id: &SurfaceId, _: &Connection, _: &QueueHandle<Self>) {
            if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
                if matches!(mode, wayland_client::WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide)) {
                    state.client_decorated.remove(&id.0);
                    state.frames.remove(&id.0);
                } else { state.client_decorated.insert(id.0); }
            }
        }
    }

    impl Dispatch<xdg_toplevel::XdgToplevel, SurfaceId> for State {
        fn event(
            state: &mut Self,
            proxy: &xdg_toplevel::XdgToplevel,
            event: xdg_toplevel::Event,
            _: &SurfaceId,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let Some(id) = proxy.data::<SurfaceId>().map(|data| data.0) else { return };
            match event {
                xdg_toplevel::Event::Close => state.request_close(id),
                xdg_toplevel::Event::Configure { width, height, states } => {
                    let mut flags = WindowState::empty();
                    for value in states.chunks_exact(4) {
                        flags |= match u32::from_ne_bytes(value.try_into().unwrap()) {
                            1 => WindowState::MAXIMIZED, 2 => WindowState::FULLSCREEN,
                            3 => WindowState::RESIZING, 4 => WindowState::ACTIVATED,
                            5 => WindowState::TILED_LEFT, 6 => WindowState::TILED_RIGHT,
                            7 => WindowState::TILED_TOP, 8 => WindowState::TILED_BOTTOM,
                            _ => WindowState::empty(),
                        };
                    }
                    state.configured.insert(id, (width, height, flags));
                }
                xdg_toplevel::Event::WmCapabilities { capabilities } => {
                    let mut flags = WindowManagerCapabilities::empty();
                    for value in capabilities.chunks_exact(4) {
                        flags |= match u32::from_ne_bytes(value.try_into().unwrap()) {
                            1 => WindowManagerCapabilities::WINDOW_MENU,
                            2 => WindowManagerCapabilities::MAXIMIZE,
                            3 => WindowManagerCapabilities::FULLSCREEN,
                            4 => WindowManagerCapabilities::MINIMIZE,
                            _ => WindowManagerCapabilities::empty(),
                        };
                    }
                    if state.viewporter.is_none() { flags.remove(WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::FULLSCREEN); }
                    state.wm_capabilities.insert(id, flags);
                    if let Some(frame) = state.frames.get_mut(&id) { frame.update_wm_capabilities(flags); }
                    state.redraw_frame(id);
                }
                _ => {}
            }
        }
    }
    #[cfg(test)]
    mod focus_tests {
        use super::*;
        use pengudroid_proto::InputKind;
        #[test]
        fn initial_portrait_window_fits_small_and_rotated_desktops() {
            assert_eq!(fit_window((720, 1280), (1000, 700)), (393, 700));
            assert_eq!(fit_window((1280, 720), (700, 1000)), (700, 393));
            assert_eq!(fit_window((320, 240), (1000, 700)), (320, 240));
        }

        #[test]
        fn hardware_video_import_preserves_planes_and_waits_for_release() {
            use pengudroid_hostgpu::video::{Decoder, ReceiveStatus, SendStatus};
            use std::sync::atomic::Ordering;
            let (Some(plugin), Some(fixture)) = (std::env::var_os("PENGUDROID_TEST_VIDEO_BRIDGE"),
                std::env::var_os("PENGUDROID_TEST_VIDEO_FIXTURE")) else { return; };
            let bytes = std::fs::read(fixture).unwrap();
            let device = std::env::var_os("PENGUDROID_TEST_VIDEO_DEVICE").unwrap_or_else(|| "/dev/dri/renderD128".into());
            let mut decoder = Decoder::open_plugin(std::path::Path::new(&plugin), "h264", std::path::Path::new(&device), &[]).unwrap();
            let mut graphics = Graphics::connect().expect("real compositor required for GPU video import test");
            graphics.refresh();
            let id = graphics.create_toplevel(128, 128, "PenguDroid hardware video test").unwrap().id;
            for pts in 0..32 {
                decoder.flush();
                assert_eq!(decoder.send(&bytes, pts).unwrap(), SendStatus::Accepted);
                assert_eq!(decoder.end_of_stream().unwrap(), SendStatus::Accepted);
                let frame = match decoder.receive().unwrap() {
                    ReceiveStatus::Frame(frame) => frame, _ => panic!("no hardware frame"),
                };
                let export = frame.export().unwrap();
                // VA-API hands over the decode surface (NV12, two planes); the
                // Vulkan Video backend hands over one RGBA plane.
                if export.fourcc == u32::from_le_bytes(*b"NV12") {
                    assert_eq!(export.planes.len(), 2, "NV12 planes must reach the compositor unchanged");
                } else {
                    assert_eq!((export.fourcc, export.planes.len()), (u32::from_le_bytes(*b"XB24"), 1));
                }
                let released = graphics.present_video_dmabuf(id, export).unwrap().expect("frame slot available");
                graphics.refresh();
                if pts == 31 { assert!(graphics.destroy_surface(id)); }
                else { assert!(graphics.present(id)); } // detach the video buffer
                for _ in 0..100 {
                    graphics.refresh();
                    if released.load(Ordering::Acquire) { break; }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert!(released.load(Ordering::Acquire), "source surface cannot be recycled before wl_buffer.release");
                drop(frame);
            }
            graphics.destroy_surface(id);
            graphics.refresh();
        }

        #[test]
        fn software_frames_remain_bounded_and_release_wayland_objects() {
            if std::env::var_os("WAYLAND_DISPLAY").is_none() { return; }
            let mut graphics = Graphics::connect().expect("desktop compositor");
            let id = graphics.create_toplevel(64, 64, "PenguDroid frame lifetime test").unwrap().id;
            let source = AshmemRegion::create("pd-frame-lifetime", 128 * 72 * 4).unwrap();
            let original = graphics.state.surfaces[&id].buffer.id();
            for _ in 0..1000 {
                graphics.present_shm(id, source.try_clone_fd().unwrap(), 128, 72, 512, 1, BufferCrop::full(128, 72)).unwrap();
                assert!(graphics.state.surfaces[&id].pending_frames.len() <= 3);
            }
            graphics.queue.roundtrip(&mut graphics.state).unwrap();
            let surface = &graphics.state.surfaces[&id];
            assert_eq!(surface.buffer.id(), original, "fallback buffer remains owned separately");
            // At most the currently attached buffer may still be retained once
            // the compositor has processed and released superseded commits.
            assert!(surface.pending_frames.iter().filter(|frame| frame.buffer.is_alive()).count() <= 1);
            graphics.destroy_surface(id);
            graphics.queue.roundtrip(&mut graphics.state).unwrap();
        }

        #[test]
        fn native_frame_resize_maximize_and_close_keep_video_and_input_aligned() {
            if std::env::var_os("WAYLAND_DISPLAY").is_none() {
                eprintln!("skipping: no Wayland compositor"); return;
            }
            let mut graphics = Graphics::connect().expect("connect to desktop compositor");
            if graphics.state.viewporter.is_none() {
                eprintln!("skipping: compositor does not provide wp_viewporter"); return;
            }
            let info = graphics.create_toplevel(320, 240, "PenguDroid desktop lifecycle test").unwrap();
            let id = info.id;
            if let Some(decoration) = graphics.state.decorations.get(&id) {
                decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
                graphics.queue.roundtrip(&mut graphics.state).unwrap();
            }
            graphics.state.client_decorated.insert(id);
            graphics.state.configured.insert(id, (640, 480, WindowState::ACTIVATED));
            graphics.state.configure_window(id, &graphics.queue.handle());
            graphics.queue.roundtrip(&mut graphics.state).unwrap();
            assert!(graphics.state.frames.contains_key(&id), "desktop frame must exist");
            let header = graphics.state.frames[&id].add_borders(0, 0).1;
            let root = &graphics.state.surfaces[&id];
            assert_eq!((root.width, root.height), (320, 240), "guest buffer remains valid");
            assert_eq!((root.display_width, root.display_height), (640, 480 - header));
            let (x, y) = graphics.state.guest_coordinates(id, 320., (480 - header) as f64 / 2.);
            assert!((x - 160.).abs() < 0.001 && (y - 120.).abs() < 0.001);
            let video = graphics.create_subsurface(id, 16, 20, 160, 90).unwrap();
            let child = &graphics.state.surfaces[&video];
            assert_eq!(child.display_width, 320);
            assert_eq!(child.display_height, (90 * (480 - header) / 240).max(1));
            let top = graphics.state.surfaces[&id].xdg_toplevel.clone().unwrap();
            for maximized in [true, false] {
                if maximized { top.set_maximized(); } else { top.unset_maximized(); }
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    graphics.queue.roundtrip(&mut graphics.state).unwrap();
                    if graphics.state.surfaces[&id].window_state.contains(WindowState::MAXIMIZED) == maximized { break; }
                    assert!(std::time::Instant::now() < deadline, "compositor did not configure maximize={maximized}");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            graphics.state.request_close(id);
            graphics.state.request_close(id);
            assert_eq!(graphics.close_requests(), vec![id]);
            assert!(!graphics.present(id), "closed windows reject new guest frames");
            graphics.destroy_surface(id);
            assert!(!graphics.state.frames.contains_key(&id));
            assert!(!graphics.contains_surface(video));
            graphics.queue.roundtrip(&mut graphics.state).unwrap();
        }

        #[test]
        fn keyboard_focus_is_independent_of_mouse_and_leave_releases_modifiers() {
            let mut state = State::default();
            state.pointer_focus = Some(10);
            state.keyboard_focus = Some(20);
            state.keyboard_key(42, true);
            state.keyboard_key(30, true);
            assert!(state.input.iter().all(|(id, _)| *id == 20));
            state.input.clear();
            state.release_keyboard();
            assert_eq!(state.input.len(), 2);
            assert!(state.input.iter().all(|(id, event)| *id == 20 && matches!(event, InputKind::Key { pressed: false, .. })));
            state.input.clear();
            state.keyboard_key(48, true);
            assert!(state.input.is_empty(), "no input after keyboard focus leaves");
        }
    }

}

#[cfg(feature = "wayland")]
pub use imp::Graphics;

#[cfg(not(feature = "wayland"))]
mod imp {
    use super::SurfaceInfo;
    /// Graphics-disabled build.
    pub struct Graphics;
    impl Graphics {
        pub fn connect() -> Option<Graphics> {
            crate::warn!("built without the `wayland` feature; graphics disabled");
            None
        }
        pub fn create_toplevel(&mut self, _w: u32, _h: u32, _t: &str) -> Option<SurfaceInfo> {
            None
        }
        pub fn create_subsurface(
            &mut self,
            _parent: u32,
            _x: i32,
            _y: i32,
            _w: u32,
            _h: u32,
        ) -> Option<u32> {
            None
        }
        pub fn set_subsurface_position(&mut self, _id: u32, _x: i32, _y: i32) -> bool {
            false
        }
        pub fn focused_surface(&self) -> Option<u32> {
            None
        }
        pub fn set_surface_options(&mut self, _id: u32, _enabled: bool) -> bool { false }
        pub fn set_window_state(&mut self, _id: u32, _state: u32) -> bool { false }
        pub fn surface_geometry(&mut self, _id: u32) -> Option<(u32, u32, u32, u32)> { None }
        pub fn set_pointer_lock(&mut self, _id: u32, _locked: bool) -> bool { false }
        pub fn pointer_lock_surface(&self) -> Option<u32> { None }
        pub fn contains_surface(&self, _id: u32) -> bool { false }
        pub fn is_toplevel(&self, _id: u32) -> bool { false }
        pub fn destroy_surface(&mut self, _id: u32) -> bool {
            false
        }
        pub fn present(&mut self, _id: u32) -> bool {
            false
        }
        pub fn poll(&mut self) {}
        pub fn poll_input(&mut self) -> Vec<(u32, pengudroid_proto::InputKind)> {
            Vec::new()
        }
        pub fn close_requests(&mut self) -> Vec<u32> {
            Vec::new()
        }
        pub fn surface_close_requested(&self, _id: u32) -> bool {
            false
        }
        pub fn surface_fd(&self, _id: u32) -> Option<std::os::fd::OwnedFd> {
            None
        }
        pub fn dmabuf_available(&self) -> bool {
            false
        }
        pub fn dmabuf_format_count(&self) -> u32 {
            0
        }
        pub fn dmabuf_supports(&self, _fourcc: u32, _modifier: u64) -> bool {
            false
        }
        pub fn refresh(&mut self) {}
        #[allow(clippy::too_many_arguments)]
        pub fn dmabuf_selftest(
            &mut self,
            _width: u32,
            _height: u32,
            _fd: std::os::fd::OwnedFd,
            _stride: u32,
            _fourcc: u32,
            _modifier: u64,
            _offset: u32,
        ) -> Result<(), String> {
            Err("graphics built without the `wayland` feature".into())
        }
        #[allow(clippy::too_many_arguments)]
        pub fn present_shm(&mut self, _id: u32, _fd: std::os::fd::OwnedFd,
            _width: u32, _height: u32, _stride: u32, _format: u32, _crop: pengudroid_proto::BufferCrop) -> Result<(), String> {
            Err("graphics built without the `wayland` feature".into())
        }
        pub fn present_video_dmabuf(&mut self, _id: u32, _frame: pengudroid_hostgpu::video::ExportedFrame)
            -> Result<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>, String> {
            Err("graphics built without the wayland feature".into())
        }
        #[allow(clippy::too_many_arguments)]
        pub fn present_dmabuf(
            &mut self,
            _id: u32,
            _fd: std::os::fd::OwnedFd,
            _width: u32,
            _height: u32,
            _stride: u32,
            _fourcc: u32,
            _modifier: u64,
            _offset: u32,
            _top_down: bool,
        ) -> Result<(), String> {
            Err("graphics built without the `wayland` feature".into())
        }
    }
}

#[cfg(not(feature = "wayland"))]
pub use imp::Graphics;
