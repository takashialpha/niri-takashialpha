use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{mem, thread};

use _server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDecorationsMode;
use anyhow::Context;
use calloop::futures::Scheduler;
use niri_config::output::MaxBpc;
use niri_config::{
    Config, FloatOrInt, Key, Modifiers, OutputName, TrackLayout, WarpMouseToFocusMode,
    WorkspaceReference, Xkb,
};
use smithay::backend::allocator::Fourcc;
use smithay::backend::input::Keycode;
use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
    select_dmabuf_feedback,
};
use smithay::backend::renderer::element::{
    Element, Id, Kind, PrimaryScanoutOutput, RenderElementStates,
    default_primary_scanout_output_compare,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::utils::{
    OutputPresentationFeedback, bbox_from_surface_tree, output_update,
    send_dmabuf_feedback_surface_tree, send_frames_surface_tree,
    surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
    take_presentation_feedback_surface_tree, under_from_surface_tree,
    update_surface_primary_scanout_output, with_surfaces_surface_tree,
};
use smithay::desktop::{
    LayerMap, LayerSurface, PopupGrab, PopupManager, PopupUngrabStrategy, Space, Window,
    WindowSurfaceType, find_popup_root_surface, layer_map_for_output,
};
use smithay::input::keyboard::{Layout as KeyboardLayout, XkbConfig};
use smithay::input::pointer::{
    CursorIcon, CursorImageStatus, CursorImageSurfaceData, Focus,
    GrabStartData as PointerGrabStartData, MotionEvent,
};
use smithay::input::{Seat, SeatState};
use smithay::output::{self, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{
    Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken,
};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities;
use smithay::reexports::wayland_protocols_misc::server_decoration as _server_decoration;
use smithay::reexports::wayland_server::backend::{
    ClientData, ClientId, DisconnectReason, GlobalId,
};
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{
    ClockSource, IsAlive as _, Logical, Monotonic, Physical, Point, Rectangle, SERIAL_COUNTER,
    Scale, Size, Transform,
};
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, HookId, SurfaceData,
    TraversalAction, with_states, with_surface_tree_downward,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_constraints::{PointerConstraintsState, with_pointer_constraint};
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::selection::data_device::{DataDeviceState, set_data_device_selection};
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::session_lock::{LockSurface, SessionLockManagerState, SessionLocker};
use smithay::wayland::shell::kde::decoration::KdeDecorationState;
use smithay::wayland::shell::wlr_layer::{self, Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xdg_foreign::XdgForeignState;
use wayland_server::protocol::wl_output::WlOutput;

use crate::animation::Clock;
use crate::backend::tty::SurfaceDmabufFeedback;
use crate::backend::{Backend, Headless, RenderResult, Tty};
use crate::cursor::{CursorManager, CursorTextureCache, RenderCursor, XCursor};
use crate::frame_clock::FrameClock;
use crate::handlers::configure_lock_surface;
use crate::input::pick_color_grab::PickColorGrab;
use crate::input::scroll_tracker::ScrollTracker;
use crate::input::{apply_libinput_settings, mods_with_mouse_binds, mods_with_wheel_binds};
use crate::ipc::server::IpcServer;
use crate::layer::MappedLayer;
use crate::layer::mapped::LayerSurfaceRenderElement;
use crate::layout::tile::TileRenderElement;
use crate::layout::workspace::{Workspace, WorkspaceId};
use crate::layout::{
    HitType, Layout, LayoutElement as _, LayoutElementRenderElement, MonitorRenderElement,
};
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::surface::push_elements_from_surface_tree;
use crate::render_helpers::texture::TextureBuffer;
use crate::render_helpers::{
    RenderCtx, RenderTarget, encompassing_geo, render_to_encompassing_texture, render_to_texture,
    render_to_vec,
};
use crate::ui::config_error_notification::ConfigErrorNotification;
use crate::ui::exit_confirm_dialog::{ExitConfirmDialog, ExitConfirmDialogRenderElement};
use crate::ui::hotkey_overlay::HotkeyOverlay;
use crate::ui::screen_transition::{self, ScreenTransition};
use crate::ui::screenshot_ui::{OutputScreenshot, ScreenshotUi, ScreenshotUiRenderElement};
use crate::utils::scale::{closest_representable_scale, guess_monitor_scale};
use crate::utils::spawning::CHILD_ENV;
use crate::utils::vblank_throttle::VBlankThrottle;
use crate::utils::watcher::Watcher;
use crate::utils::{
    center, center_f64, expand_home, get_monotonic_time, ipc_transform_to_smithay, is_mapped,
    logical_output, make_screenshot_path, output_matches_name, output_size, panel_orientation,
    send_scale_transform, write_png_rgba8,
};
use crate::window::mapped::MappedId;
use crate::window::{InitialConfigureState, Mapped, ResolvedWindowRules, Unmapped, WindowRef};

const CLEAR_COLOR_LOCKED: [f32; 4] = [0.3, 0.1, 0.1, 1.];

// We'll try to send frame callbacks at least once a second. We'll make a timer that fires once a
// second, so with the worst timing the maximum interval between two frame callbacks for a surface
// should be ~1.995 seconds.
const FRAME_CALLBACK_THROTTLE: Option<Duration> = Some(Duration::from_millis(995));

// Wayland input/presentation event timestamps are u32 milliseconds since an unspecified
// epoch by protocol definition (e.g. wl_pointer.motion's `time` argument): clients are
// expected to only use them for computing relative deltas, and wrapping every ~49.7 days is
// normal, protocol-mandated behavior rather than a bug, so the truncation here is intentional.
#[allow(clippy::cast_possible_truncation)]
fn wayland_time_now() -> u32 {
    get_monotonic_time().as_millis() as u32
}

// Bundling these into bitflags would touch every field access across the whole compositor;
// each bool tracks an independently-toggled piece of state, not a combinable flag set.
#[allow(clippy::struct_excessive_bools)]
pub struct Niri {
    pub config: Rc<RefCell<Config>>,

    /// Output config from the config file.
    ///
    /// This does not include transient output config changes done via IPC. It is only used when
    /// reloading the config from disk to determine if the output configuration should be reloaded
    /// (and transient changes dropped).
    pub config_file_output_config: niri_config::Outputs,

    pub config_file_watcher: Option<Watcher>,

    pub event_loop: LoopHandle<'static, State>,
    pub scheduler: Scheduler<()>,
    pub stop_signal: LoopSignal,
    pub display_handle: DisplayHandle,

    /// Name of the Wayland socket.
    ///
    /// This is `None` when creating `Niri` without a Wayland socket.
    pub socket_name: Option<OsString>,

    pub start_time: Instant,

    /// Whether the at-startup=true window rules are active.
    pub is_at_startup: bool,

    /// Clock for driving animations.
    pub clock: Clock,

    // Each workspace corresponds to a Space. Each workspace generally has one Output mapped to it,
    // however it may have none (when there are no outputs connected) or multiple (when mirroring).
    pub layout: Layout<Mapped>,

    // This space does not actually contain any windows, but all outputs are mapped into it
    // according to their global position.
    pub global_space: Space<Window>,

    /// Mapped outputs, sorted by their name and position.
    pub sorted_outputs: Vec<Output>,

    // Windows which don't have a buffer attached yet.
    pub unmapped_windows: HashMap<WlSurface, Unmapped>,

    /// Layer surfaces which don't have a buffer attached yet.
    pub unmapped_layer_surfaces: HashSet<WlSurface>,

    /// Extra data for mapped layer surfaces.
    pub mapped_layer_surfaces: HashMap<LayerSurface, MappedLayer>,

    // Cached root surface for every surface, so that we can access it in destroyed() where the
    // normal get_parent() is cleared out.
    pub root_surface: HashMap<WlSurface, WlSurface>,

    // Dmabuf readiness pre-commit hook for a surface.
    pub dmabuf_pre_commit_hook: HashMap<WlSurface, HookId>,

    /// Clients to notify about their blockers being cleared.
    pub blocker_cleared_tx: Sender<Client>,
    pub blocker_cleared_rx: Receiver<Client>,

    pub output_state: HashMap<Output, OutputState>,

    // When false, we're idling with monitors powered off.
    pub monitors_active: bool,

    pub devices: HashSet<input::Device>,

    // Smithay state.
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub kde_decoration_state: KdeDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub session_lock_state: SessionLockManagerState,
    pub viewporter_state: ViewporterState,
    pub xdg_foreign_state: XdgForeignState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub dmabuf_state: DmabufState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub seat_state: SeatState<State>,
    pub relative_pointer_state: RelativePointerManagerState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub popups: PopupManager,
    pub popup_grab: Option<PopupGrabState>,
    pub presentation_state: PresentationState,

    pub seat: Seat<State>,
    /// Scancodes of the keys to suppress.
    pub suppressed_keys: HashSet<Keycode>,
    /// Button codes of the mouse buttons to suppress.
    pub suppressed_buttons: HashSet<u32>,
    pub bind_cooldown_timers: HashMap<Key, RegistrationToken>,
    pub bind_repeat_timer: Option<RegistrationToken>,
    pub keyboard_focus: KeyboardFocus,
    pub layer_shell_on_demand_focus: Option<LayerSurface>,

    /// Most recent XKB settings from org.freedesktop.locale1.
    pub xkb_from_locale1: Option<Xkb>,

    pub cursor_manager: CursorManager,
    pub cursor_texture_cache: CursorTextureCache,
    pub cursor_shape_manager_state: CursorShapeManagerState,
    pub dnd_icon: Option<DndIcon>,
    /// Contents under pointer.
    ///
    /// Periodically updated: on motion and other events and in the loop callback. If you require
    /// the real up-to-date contents somewhere, it's better to recompute on the spot.
    ///
    /// This is not pointer focus. I.e. during a click grab, the pointer focus remains on the
    /// client with the grab, but this field will keep updating to the latest contents as if no
    /// grab was active.
    ///
    /// This is primarily useful for emitting pointer motion events for surfaces that move
    /// underneath the cursor on their own (i.e. when the tiling layout moves). In this case, not
    /// taking grabs into account is expected, because we pass the information to `pointer.motion()`
    /// which passes it down through grabs, which decide what to do with it as they see fit.
    pub pointer_contents: PointContents,
    pub pointer_visibility: PointerVisibility,
    pub pointer_inactivity_timer: Option<RegistrationToken>,
    /// Whether the pointer inactivity timer got reset this event loop iteration.
    ///
    /// Used for limiting the reset to once per iteration, so that it's not spammed with high
    /// resolution mice.
    pub pointer_inactivity_timer_got_reset: bool,
    /// Whether the (idle notifier) activity was notified this event loop iteration.
    ///
    /// Used for limiting the notify to once per iteration, so that it's not spammed with high
    /// resolution mice.
    pub notified_activity_this_iteration: bool,
    pub pointer_inside_hot_corner: bool,
    pub vertical_wheel_tracker: ScrollTracker,
    pub horizontal_wheel_tracker: ScrollTracker,
    pub mods_with_mouse_binds: HashSet<Modifiers>,
    pub mods_with_wheel_binds: HashSet<Modifiers>,

    pub lock_state: LockState,

    pub screenshot_ui: ScreenshotUi,
    pub config_error_notification: ConfigErrorNotification,
    pub hotkey_overlay: HotkeyOverlay,
    pub exit_confirm_dialog: ExitConfirmDialog,

    pub pick_window: Option<async_channel::Sender<Option<MappedId>>>,
    pub pick_color: Option<async_channel::Sender<Option<niri_ipc::PickedColor>>>,

    pub ipc_server: Option<IpcServer>,
    pub ipc_outputs_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerVisibility {
    /// The pointer is visible.
    Visible,
    /// The pointer is invisible, but retains its focus.
    ///
    /// This state is set temporarily after auto-hiding the pointer to keep tooltips open and grabs
    /// ongoing.
    Hidden,
    /// The pointer is invisible and cannot focus.
    ///
    /// Corresponds to a fully disabled pointer, for example after a touchscreen input, or after
    /// the pointer contents changed in a Hidden state.
    Disabled,
}

impl PointerVisibility {
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        matches!(self, Self::Visible)
    }
}

#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

pub struct OutputState {
    pub global: GlobalId,
    pub frame_clock: FrameClock,
    pub redraw_state: RedrawState,
    // After the last redraw, some ongoing animations still remain.
    pub unfinished_animations_remain: bool,
    pub vblank_throttle: VBlankThrottle,
    /// Sequence for frame callback throttling.
    ///
    /// We want to send frame callbacks for each surface at most once per monitor refresh cycle.
    ///
    /// Even if a surface commit resulted in empty damage to the monitor, we want to delay the next
    /// frame callback until roughly when a `VBlank` would occur, had the monitor been damaged. This
    /// is necessary to prevent clients busy-looping with frame callbacks that result in empty
    /// damage.
    ///
    /// This counter wrapping-increments by 1 every time we move into the next refresh cycle, as
    /// far as frame callback throttling is concerned. Specifically, it happens:
    ///
    /// 1. Upon a successful DRM frame submission. Notably, we don't wait for the `VBlank` here,
    ///    because the client buffers are already "latched" at the point of submission. Even if a
    ///    client submits a new buffer right away, we will wait for a `VBlank` to draw it, which
    ///    means that busy looping is avoided.
    /// 2. If a frame resulted in empty damage, a timer is queued to fire roughly when a `VBlank`
    ///    would occur, based on the last presentation time and output refresh interval. Sequence
    ///    is incremented in that timer, before attempting a redraw or sending frame callbacks.
    pub frame_callback_sequence: u32,
    /// Solid color buffer for the backdrop that we use instead of clearing to avoid damage
    /// tracking issues and make screenshots easier.
    pub backdrop_buffer: SolidColorBuffer,
    pub lock_render_state: LockRenderState,
    pub lock_surface: Option<LockSurface>,
    pub lock_color_buffer: SolidColorBuffer,
    screen_transition: Option<ScreenTransition>,
}

#[derive(Debug, Default)]
pub enum RedrawState {
    /// The compositor is idle.
    #[default]
    Idle,
    /// A redraw is queued.
    Queued,
    /// We submitted a frame to the KMS and waiting for it to be presented.
    WaitingForVBlank { redraw_needed: bool },
    /// We did not submit anything to KMS and made a timer to fire at the estimated `VBlank`.
    WaitingForEstimatedVBlank(RegistrationToken),
    /// A redraw is queued on top of the above.
    WaitingForEstimatedVBlankAndQueued(RegistrationToken),
}

pub struct PopupGrabState {
    pub root: WlSurface,
    pub grab: PopupGrab<State>,
    pub has_keyboard_grab: bool,
}

// The surfaces here are always toplevel surfaces focused as far as niri's logic is concerned, even
// when popup grabs are active (which means the real keyboard focus is on a popup descending from
// that toplevel surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyboardFocus {
    // Layout is focused by default if there's nothing else to focus.
    Layout { surface: Option<WlSurface> },
    LayerShell { surface: WlSurface },
    LockScreen { surface: Option<WlSurface> },
    ScreenshotUi,
    ExitConfirmDialog,
    Overview,
}

#[derive(Default, Clone, PartialEq)]
pub struct PointContents {
    // Output under point.
    pub output: Option<Output>,
    // Surface under point and its location in the global coordinate space.
    //
    // Can be `None` even when `window` is set, for example when the pointer is over the niri
    // border around the window.
    pub surface: Option<(WlSurface, Point<f64, Logical>)>,
    // If surface belongs to a window, this is that window.
    pub window: Option<(Window, HitType)>,
    // If surface belongs to a layer surface, this is that layer surface.
    pub layer: Option<LayerSurface>,
    // Pointer is over a hot corner.
    pub hot_corner: bool,
}

#[derive(Debug, Default)]
pub enum LockState {
    #[default]
    Unlocked,
    WaitingForSurfaces {
        confirmation: SessionLocker,
        deadline_token: RegistrationToken,
    },
    Locking(SessionLocker),
    Locked(ExtSessionLockV1),
}

#[derive(PartialEq, Eq)]
pub enum LockRenderState {
    /// The output displays a normal session frame.
    Unlocked,
    /// The output displays a locked frame.
    Locked,
}

// Not related to the one in Smithay.
//
// This state keeps track of when a surface last received a frame callback.
struct SurfaceFrameThrottlingState {
    /// Output and sequence that the frame callback was last sent at.
    last_sent_at: RefCell<Option<(Output, u32)>>,
}

#[derive(Clone, Copy)]
pub enum CenterCoords {
    Separately,
    Both,
    // Force centering even if the cursor is already in the rectangle.
    BothAlways,
}

impl RedrawState {
    const fn queue_redraw(self) -> Self {
        match self {
            Self::Idle => Self::Queued,
            Self::WaitingForEstimatedVBlank(token) => {
                Self::WaitingForEstimatedVBlankAndQueued(token)
            }

            // A redraw is already queued.
            value @ (Self::Queued | Self::WaitingForEstimatedVBlankAndQueued(_)) => value,

            // We're waiting for VBlank, request a redraw afterwards.
            Self::WaitingForVBlank { .. } => Self::WaitingForVBlank {
                redraw_needed: true,
            },
        }
    }
}

impl Default for SurfaceFrameThrottlingState {
    fn default() -> Self {
        Self {
            last_sent_at: RefCell::new(None),
        }
    }
}

impl KeyboardFocus {
    #[must_use]
    pub const fn surface(&self) -> Option<&WlSurface> {
        match self {
            Self::Layout { surface } | Self::LockScreen { surface } => surface.as_ref(),
            Self::LayerShell { surface } => Some(surface),
            Self::ScreenshotUi | Self::ExitConfirmDialog | Self::Overview => None,
        }
    }

    #[must_use]
    pub fn into_surface(self) -> Option<WlSurface> {
        match self {
            Self::Layout { surface } | Self::LockScreen { surface } => surface,
            Self::LayerShell { surface } => Some(surface),
            Self::ScreenshotUi | Self::ExitConfirmDialog | Self::Overview => None,
        }
    }

    #[must_use]
    pub const fn is_layout(&self) -> bool {
        matches!(self, Self::Layout { .. })
    }

    #[must_use]
    pub const fn is_overview(&self) -> bool {
        matches!(self, Self::Overview)
    }
}

pub struct State {
    pub backend: Backend,
    pub niri: Niri,
}

impl State {
    /// # Errors
    ///
    /// Errors if initializing the TTY backend fails (not applicable when `headless` is `true`).
    pub fn new(
        config: Config,
        event_loop: LoopHandle<'static, Self>,
        stop_signal: LoopSignal,
        display: Display<Self>,
        headless: bool,
        create_wayland_socket: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let config = Rc::new(RefCell::new(config));

        let mut backend = if headless {
            let headless = Headless::new();
            Backend::Headless(headless)
        } else {
            let tty = Tty::new(config.clone(), &event_loop)
                .context("error initializing the TTY backend")?;
            Backend::Tty(tty)
        };

        let mut niri = Niri::new(
            config,
            event_loop,
            stop_signal,
            display,
            &backend,
            create_wayland_socket,
        );
        backend.init(&mut niri);

        let mut state = Self { backend, niri };

        // Load the xkb_file config option if set by the user.
        state.load_xkb_file();
        // Initialize some IPC server state.
        state.ipc_keyboard_layouts_changed();
        // Focus the default monitor if set by the user.
        state.focus_default_monitor();

        Ok(state)
    }

    /// # Panics
    ///
    /// Panics if flushing the Wayland display's clients fails.
    pub fn refresh_and_flush_clients(&mut self) {
        self.refresh();

        // Advance animations to the current time (not target render time) before rendering outputs
        // in order to clear completed animations and render elements. Even if we're not rendering,
        // it's good to advance every now and then so the workspace clean-up and animations don't
        // build up (the 1 second frame callback timer will call this line).
        self.niri.advance_animations();

        self.niri.redraw_queued_outputs(&mut self.backend);

        {
            self.niri.display_handle.flush_clients().unwrap();
        }

        // Clear the time so it's fetched afresh next iteration.
        self.niri.clock.clear();
        self.niri.pointer_inactivity_timer_got_reset = false;
        self.niri.notified_activity_this_iteration = false;
    }

    fn refresh(&mut self) {
        // Handle commits for surfaces whose blockers cleared this cycle. This should happen before
        // layout.refresh() since this is where these surfaces handle commits.
        self.notify_blocker_cleared();

        // These should be called periodically, before flushing the clients.
        self.niri.popups.cleanup();
        self.refresh_popup_grab();
        self.update_keyboard_focus();

        // Should be called before refresh_layout() because that one will refresh other window
        // states and then send a pending configure.
        self.niri.refresh_window_states();

        // Needs to be called after updating the keyboard focus.
        self.niri.refresh_layout();

        self.niri.cursor_manager.check_cursor_image_surface_alive();
        self.niri.refresh_pointer_outputs();
        self.niri.global_space.refresh();
        self.refresh_pointer_contents();

        self.niri.refresh_window_rules();
        self.refresh_ipc_outputs();
        self.ipc_refresh_layout();
        self.ipc_refresh_keyboard_layout_index();

        // Needs to be called after updating the keyboard focus.
    }

    fn notify_blocker_cleared(&mut self) {
        let dh = self.niri.display_handle.clone();
        while let Ok(client) = self.niri.blocker_cleared_rx.try_recv() {
            trace!("calling blocker_cleared");
            self.client_compositor_state(&client)
                .blocker_cleared(self, &dh);
        }
    }

    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn move_cursor(&mut self, location: Point<f64, Logical>) {
        let mut under = match self.niri.pointer_visibility {
            PointerVisibility::Disabled => PointContents::default(),
            _ => self.niri.contents_under(location),
        };

        // Disable the hidden pointer if the contents underneath have changed.
        if !self.niri.pointer_visibility.is_visible() && self.niri.pointer_contents != under {
            self.niri.pointer_visibility = PointerVisibility::Disabled;

            // When setting PointerVisibility::Hidden together with pointer contents changing,
            // we can change straight to nothing to avoid one frame of hover. Notably, this can
            // be triggered through warp-mouse-to-focus combined with hide-when-typing.
            under = PointContents::default();
        }

        self.niri.pointer_contents.clone_from(&under);

        let pointer = &self.niri.seat.get_pointer().unwrap();
        pointer.motion(
            self,
            under.surface,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: wayland_time_now(),
            },
        );
        pointer.frame(self);

        self.niri.maybe_activate_pointer_constraint();

        // We do not show the pointer on programmatic or keyboard movement.

        // FIXME: granular
        self.niri.queue_redraw_all();
    }

    /// Moves cursor within the specified rectangle, only adjusting coordinates if needed.
    fn move_cursor_to_rect(&mut self, rect: Rectangle<f64, Logical>, mode: CenterCoords) -> bool {
        let pointer = &self.niri.seat.get_pointer().unwrap();
        let cur_loc = pointer.current_location();
        let x_in_bound = cur_loc.x >= rect.loc.x && cur_loc.x <= rect.loc.x + rect.size.w;
        let y_in_bound = cur_loc.y >= rect.loc.y && cur_loc.y <= rect.loc.y + rect.size.h;

        let p = match mode {
            CenterCoords::Separately => {
                if x_in_bound && y_in_bound {
                    return false;
                } else if y_in_bound {
                    // adjust x
                    Point::from((rect.loc.x + rect.size.w / 2.0, cur_loc.y))
                } else if x_in_bound {
                    // adjust y
                    Point::from((cur_loc.x, rect.loc.y + rect.size.h / 2.0))
                } else {
                    // adjust x and y
                    center_f64(rect)
                }
            }
            CenterCoords::Both => {
                if x_in_bound && y_in_bound {
                    return false;
                }
                // adjust x and y
                center_f64(rect)
            }
            CenterCoords::BothAlways => center_f64(rect),
        };

        self.move_cursor(p);
        true
    }

    /// # Panics
    ///
    /// Panics if:
    /// - The output has no known geometry in the global space
    /// - `output` has no associated monitor in the layout
    pub fn move_cursor_to_focused_tile(&mut self, mode: CenterCoords) -> bool {
        if !self.niri.keyboard_focus.is_layout() {
            return false;
        }

        let Some(output) = self.niri.layout.active_output() else {
            return false;
        };
        let monitor = self.niri.layout.monitor_for_output(output).unwrap();

        let mut rv = false;
        let rect = monitor.active_window_visual_rectangle();

        if let Some(rect) = rect {
            let output_geo = self.niri.global_space.output_geometry(output).unwrap();
            let mut rect = rect;
            rect.loc += output_geo.loc.to_f64();
            rv = self.move_cursor_to_rect(rect, mode);
        }

        rv
    }

    pub fn focus_default_monitor(&mut self) {
        // Our default target is the first output in sorted order.
        let Some(mut target) = self.niri.sorted_outputs.first().cloned() else {
            // No outputs are connected.
            return;
        };

        let config = self.niri.config.borrow();
        for config in &config.outputs.0 {
            if !config.focus_at_startup {
                continue;
            }
            if let Some(output) = self.niri.output_by_name_match(&config.name) {
                target = output.clone();
                break;
            }
        }
        drop(config);

        self.niri.layout.focus_output(&target);
        self.move_cursor_to_output(&target);
    }

    /// Focus a specific window, taking care of a potential active output change and cursor
    /// warp.
    ///
    /// # Panics
    ///
    /// Panics if the layout reports a new active output but that output cannot be moved to.
    pub fn focus_window(&mut self, window: &Window) {
        let active_output = self.niri.layout.active_output().cloned();

        self.niri.layout.activate_window(window);

        let new_active = self.niri.layout.active_output().cloned();
        if new_active == active_output {
            self.maybe_warp_cursor_to_focus();
        } else if !self.maybe_warp_cursor_to_focus_centered() {
            self.move_cursor_to_output(&new_active.unwrap());
        }

        // FIXME: granular
        self.niri.queue_redraw_all();
    }

    pub fn maybe_warp_cursor_to_focus(&mut self) -> bool {
        let focused = match self.niri.config.borrow().input.warp_mouse_to_focus {
            None => return false,
            Some(inner) => match inner.mode {
                None => CenterCoords::Separately,
                Some(WarpMouseToFocusMode::CenterXy) => CenterCoords::Both,
                Some(WarpMouseToFocusMode::CenterXyAlways) => CenterCoords::BothAlways,
            },
        };
        self.move_cursor_to_focused_tile(focused)
    }

    pub fn maybe_warp_cursor_to_focus_centered(&mut self) -> bool {
        let focused = match self.niri.config.borrow().input.warp_mouse_to_focus {
            None => return false,
            Some(inner) => match inner.mode {
                None | Some(WarpMouseToFocusMode::CenterXy) => CenterCoords::Both,
                Some(WarpMouseToFocusMode::CenterXyAlways) => CenterCoords::BothAlways,
            },
        };
        self.move_cursor_to_focused_tile(focused)
    }

    /// # Panics
    ///
    /// Panics if:
    /// - The seat has no pointer capability (niri always attaches one at startup)
    /// - `output` has no associated monitor in the layout
    pub fn refresh_pointer_contents(&mut self) {
        let pointer = &self.niri.seat.get_pointer().unwrap();
        let location = pointer.current_location();

        if !self.niri.exit_confirm_dialog.is_open()
            && !self.niri.is_locked()
            && !self.niri.screenshot_ui.is_open()
        {
            // Don't refresh cursor focus during transitions.
            if let Some((output, _)) = self.niri.output_under(location) {
                let monitor = self.niri.layout.monitor_for_output(output).unwrap();
                if monitor.are_transitions_ongoing() {
                    return;
                }
            }
        }

        if !self.update_pointer_contents() {
            return;
        }

        pointer.frame(self);

        // Pointer motion from a surface to nothing triggers a cursor change to default, which
        // means we may need to redraw.

        // FIXME: granular
        self.niri.queue_redraw_all();
    }

    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn update_pointer_contents(&mut self) -> bool {
        let pointer = &self.niri.seat.get_pointer().unwrap();
        let location = pointer.current_location();
        let mut under = match self.niri.pointer_visibility {
            PointerVisibility::Disabled => PointContents::default(),
            _ => self.niri.contents_under(location),
        };

        // We're not changing the global cursor location here, so if the contents did not change,
        // then nothing changed.
        if self.niri.pointer_contents == under {
            return false;
        }

        // Disable the hidden pointer if the contents underneath have changed.
        if !self.niri.pointer_visibility.is_visible() {
            self.niri.pointer_visibility = PointerVisibility::Disabled;

            // When setting PointerVisibility::Hidden together with pointer contents changing,
            // we can change straight to nothing to avoid one frame of hover. Notably, this can
            // be triggered through warp-mouse-to-focus combined with hide-when-typing.
            under = PointContents::default();
            if self.niri.pointer_contents == under {
                return false;
            }
        }

        self.niri.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: wayland_time_now(),
            },
        );

        self.niri.maybe_activate_pointer_constraint();

        true
    }

    /// # Panics
    ///
    /// Panics if the output has no known geometry in the global space.
    pub fn move_cursor_to_output(&mut self, output: &Output) {
        let geo = self.niri.global_space.output_geometry(output).unwrap();
        self.move_cursor(center(geo).to_f64());
    }

    pub fn refresh_popup_grab(&mut self) {
        if let Some(grab) = &mut self.niri.popup_grab
            && grab.grab.has_ended()
        {
            self.niri.popup_grab = None;
        }
    }

    /// # Panics
    ///
    /// Panics if:
    /// - The seat has no pointer capability (niri always attaches one at startup)
    /// - The seat has no keyboard capability (niri always attaches one at startup)
    /// - `output` has no associated monitor in the layout
    /// - An internal mutex is poisoned
    // This computes a single `focus` value through one sequential if/else-if chain over
    // exit-confirm/lock-screen/screenshot-ui/layer-shell/layout precedence, with several
    // closures capturing `self`, `layers`, and `layer_grab` by reference; splitting it would
    // mean threading all of those borrows through extra function parameters for no gain in
    // clarity, since each branch is only meaningful in the context of the ones before it.
    #[allow(clippy::too_many_lines)]
    pub fn update_keyboard_focus(&mut self) {
        // Clean up on-demand layer surface focus if necessary.
        if let Some(surface) = &self.niri.layer_shell_on_demand_focus {
            // Still alive and has on-demand interactivity.
            let mut good = surface.alive()
                && surface.cached_state().keyboard_interactivity
                    == wlr_layer::KeyboardInteractivity::OnDemand;

            if let Some(mapped) = self.niri.mapped_layer_surfaces.get(surface) {
                // Check if it moved to the overview backdrop.
                if mapped.place_within_backdrop() {
                    good = false;
                }
            } else {
                // The layer surface is alive but it got unmapped.
                good = false;
            }

            if !good {
                self.niri.layer_shell_on_demand_focus = None;
            }
        }

        // Compute the current focus.
        let focus = if self.niri.exit_confirm_dialog.is_open() {
            KeyboardFocus::ExitConfirmDialog
        } else if self.niri.is_locked() {
            KeyboardFocus::LockScreen {
                surface: self.niri.lock_surface_focus(),
            }
        } else if self.niri.screenshot_ui.is_open() {
            KeyboardFocus::ScreenshotUi
        } else if let Some(output) = self.niri.layout.active_output() {
            let mon = self.niri.layout.monitor_for_output(output).unwrap();
            let layers = layer_map_for_output(output);

            // Explicitly check for layer-shell popup grabs here, our keyboard focus will stay on
            // the root layer surface while it has grabs.
            let layer_grab = self.niri.popup_grab.as_ref().and_then(|g| {
                layers
                    .layer_for_surface(&g.root, WindowSurfaceType::TOPLEVEL)
                    .and_then(|l| l.can_receive_keyboard_focus().then(|| (&g.root, l.layer())))
            });
            let grab_on_layer = |layer: Layer| {
                layer_grab
                    .and_then(move |(s, l)| if l == layer { Some(s.clone()) } else { None })
                    .map(|surface| KeyboardFocus::LayerShell { surface })
            };

            let layout_focus = || {
                self.niri
                    .layout
                    .focus()
                    .map(|win| win.toplevel().wl_surface().clone())
                    .map(|surface| KeyboardFocus::Layout {
                        surface: Some(surface),
                    })
            };

            let excl_focus_on_layer = |layer| {
                layers.layers_on(layer).find_map(|surface| {
                    if surface.cached_state().keyboard_interactivity
                        != wlr_layer::KeyboardInteractivity::Exclusive
                    {
                        return None;
                    }

                    let mapped = self.niri.mapped_layer_surfaces.get(surface)?;
                    if mapped.place_within_backdrop() {
                        return None;
                    }

                    let surface = surface.wl_surface().clone();
                    Some(KeyboardFocus::LayerShell { surface })
                })
            };

            let on_d_focus_on_layer = |layer| {
                layers.layers_on(layer).find_map(|surface| {
                    let is_on_demand_surface =
                        Some(surface) == self.niri.layer_shell_on_demand_focus.as_ref();
                    is_on_demand_surface
                        .then(|| surface.wl_surface().clone())
                        .map(|surface| KeyboardFocus::LayerShell { surface })
                })
            };

            // Prefer exclusive focus on a layer, then check on-demand focus.
            let focus_on_layer =
                |layer| excl_focus_on_layer(layer).or_else(|| on_d_focus_on_layer(layer));

            let is_overview_open = self.niri.layout.is_overview_open();

            let mut surface = grab_on_layer(Layer::Overlay);
            // FIXME: we shouldn't prioritize the top layer grabs over regular overlay input or a
            // fullscreen layout window. This will need tracking in grab() to avoid handing it out
            // in the first place. Or a better way to structure this code.
            surface = surface.or_else(|| grab_on_layer(Layer::Top));

            if !is_overview_open {
                surface = surface.or_else(|| grab_on_layer(Layer::Bottom));
                surface = surface.or_else(|| grab_on_layer(Layer::Background));
            }

            surface = surface.or_else(|| focus_on_layer(Layer::Overlay));

            if mon.render_above_top_layer() {
                surface = surface.or_else(layout_focus);
                surface = surface.or_else(|| focus_on_layer(Layer::Top));
                surface = surface.or_else(|| focus_on_layer(Layer::Bottom));
                surface = surface.or_else(|| focus_on_layer(Layer::Background));
            } else {
                surface = surface.or_else(|| focus_on_layer(Layer::Top));

                if is_overview_open {
                    surface = Some(surface.unwrap_or(KeyboardFocus::Overview));
                }

                surface = surface.or_else(|| on_d_focus_on_layer(Layer::Bottom));
                surface = surface.or_else(|| on_d_focus_on_layer(Layer::Background));
                surface = surface.or_else(layout_focus);

                // Bottom and background layers can only receive exclusive focus when there are no
                // layout windows.
                surface = surface.or_else(|| excl_focus_on_layer(Layer::Bottom));
                surface = surface.or_else(|| excl_focus_on_layer(Layer::Background));
            }

            surface.unwrap_or(KeyboardFocus::Layout { surface: None })
        } else {
            KeyboardFocus::Layout { surface: None }
        };

        let keyboard = self.niri.seat.get_keyboard().unwrap();
        if self.niri.keyboard_focus != focus {
            trace!(
                "keyboard focus changed from {:?} to {:?}",
                self.niri.keyboard_focus, focus
            );

            // Tell the windows their new focus state for window rule purposes.
            if let KeyboardFocus::Layout {
                surface: Some(surface),
            } = &self.niri.keyboard_focus
                && let Some((mapped, _)) = self.niri.layout.find_window_and_output_mut(surface)
            {
                mapped.set_is_focused(false);
            }
            if let KeyboardFocus::Layout {
                surface: Some(surface),
            } = &focus
                && let Some((mapped, _)) = self.niri.layout.find_window_and_output_mut(surface)
            {
                mapped.set_is_focused(true);

                // Record when this window was last focused, used by focus-window-previous.
                mapped.set_focus_timestamp(get_monotonic_time());
            }

            if let Some(grab) = self.niri.popup_grab.as_mut()
                && grab.has_keyboard_grab
                && Some(&grab.root) != focus.surface()
            {
                trace!(
                    "grab root {:?} is not the new focus {:?}, ungrabbing",
                    grab.root, focus
                );

                grab.grab.ungrab(PopupUngrabStrategy::All);
                keyboard.unset_grab(self);
                self.niri.seat.get_pointer().unwrap().unset_grab(
                    self,
                    SERIAL_COUNTER.next_serial(),
                    wayland_time_now(),
                );
                self.niri.popup_grab = None;
            }

            if self.niri.config.borrow().input.keyboard.track_layout == TrackLayout::Window {
                let current_layout = keyboard.with_xkb_state(self, |context| {
                    let xkb = context.xkb().lock().unwrap();
                    xkb.active_layout()
                });

                let mut new_layout = current_layout;
                // Store the currently active layout for the surface.
                if let Some(current_focus) = self.niri.keyboard_focus.surface() {
                    with_states(current_focus, |data| {
                        let cell = data
                            .data_map
                            .get_or_insert::<Cell<KeyboardLayout>, _>(Cell::default);
                        cell.set(current_layout);
                    });
                }

                if let Some(focus) = focus.surface() {
                    new_layout = with_states(focus, |data| {
                        let cell = data.data_map.get_or_insert::<Cell<KeyboardLayout>, _>(|| {
                            // The default layout is effectively the first layout in the
                            // keymap, so use it for new windows.
                            Cell::new(KeyboardLayout::default())
                        });
                        cell.get()
                    });
                }
                if new_layout != current_layout && focus.surface().is_some() {
                    keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                    keyboard.with_xkb_state(self, |mut context| {
                        context.set_layout(new_layout);
                    });
                }
            }

            self.niri.keyboard_focus.clone_from(&focus);
            keyboard.set_focus(self, focus.into_surface(), SERIAL_COUNTER.next_serial());

            // FIXME: can be more granular.
            self.niri.queue_redraw_all();
        }
    }

    /// Loads the xkb keymap from a file config setting.
    fn set_xkb_file(&mut self, xkb_file: String) -> anyhow::Result<()> {
        let xkb_file = PathBuf::from(xkb_file);
        let xkb_file = expand_home(&xkb_file)
            .context("failed to expand ~")?
            .unwrap_or(xkb_file);

        let keymap = std::fs::read_to_string(xkb_file).context("failed to read xkb_file")?;

        let keyboard = self.niri.seat.get_keyboard().unwrap();
        let num_lock = keyboard.modifier_state().num_lock;

        keyboard
            .set_keymap_from_string(self, keymap)
            .context("failed to set keymap")?;

        // Restore num lock to its previous value.
        let mut mods_state = keyboard.modifier_state();
        if mods_state.num_lock != num_lock {
            mods_state.num_lock = num_lock;
            keyboard.set_modifier_state(mods_state);
        }

        Ok(())
    }

    fn load_xkb_file(&mut self) {
        let xkb_file = self.niri.config.borrow().input.keyboard.xkb.file.clone();
        if let Some(xkb_file) = xkb_file
            && let Err(err) = self.set_xkb_file(xkb_file)
        {
            warn!("error loading xkb_file: {err:?}");
        }
    }

    /// # Panics
    ///
    /// Panics if the seat has no keyboard capability (niri always attaches one at startup).
    pub fn set_xkb_config(&mut self, xkb: XkbConfig) {
        let keyboard = self.niri.seat.get_keyboard().unwrap();
        let num_lock = keyboard.modifier_state().num_lock;
        if let Err(err) = keyboard.set_xkb_config(self, xkb) {
            warn!("error updating xkb config: {err:?}");
            return;
        }

        // Restore num lock to its previous value.
        let mut mods_state = keyboard.modifier_state();
        if mods_state.num_lock != num_lock {
            mods_state.num_lock = num_lock;
            keyboard.set_modifier_state(mods_state);
        }
    }

    /// # Panics
    ///
    /// Panics if:
    /// - The seat has no keyboard capability (niri always attaches one at startup)
    /// - An internal lock is poisoned
    // This applies a new config to ~15 independent subsystems (workspaces, layout, layer
    // surfaces, keyboard, cursor, outputs, animations, ...) in a specific sequence where later
    // steps depend on earlier ones having already run (see e.g. the keyboard-focus-order
    // comments below); extracting helpers per subsystem would require passing config and
    // several `&mut self` fields around and risks silently changing that ordering.
    #[allow(clippy::too_many_lines)]
    pub fn reload_config(&mut self, config: Result<Config, ()>) {
        let Ok(mut config) = config else {
            self.niri.config_error_notification.show();
            self.niri.queue_redraw_all();

            return;
        };

        self.niri.config_error_notification.hide();

        // Find & orphan removed named workspaces.
        let mut removed_workspaces: Vec<String> = vec![];
        for ws in &self.niri.config.borrow().workspaces {
            if !config.workspaces.iter().any(|w| w.name == ws.name) {
                removed_workspaces.push(ws.name.0.clone());
            }
        }
        for name in removed_workspaces {
            self.niri.layout.unname_workspace(&name);
        }

        self.niri.layout.update_config(&config);
        for mapped in self.niri.mapped_layer_surfaces.values_mut() {
            mapped.update_config(&config);
        }

        // Create new named workspaces.
        for ws_config in &config.workspaces {
            self.niri.layout.ensure_named_workspace(ws_config);
        }

        let rate = 1.0 / config.animations.slowdown.max(0.001);
        self.niri.clock.set_rate(rate);
        self.niri
            .clock
            .set_complete_instantly(config.animations.off);

        *CHILD_ENV.write().unwrap() = mem::take(&mut config.environment);

        let mut reload_xkb = None;
        let mut libinput_config_changed = false;
        let mut output_config_changed = false;
        let mut preserved_output_config = None;
        let mut window_rules_changed = false;
        let mut layer_rules_changed = false;
        let mut cursor_inactivity_timeout_changed = false;
        let mut old_config = self.niri.config.borrow_mut();

        // Reload the cursor.
        if config.cursor != old_config.cursor {
            self.niri
                .cursor_manager
                .reload(&config.cursor.xcursor_theme, config.cursor.xcursor_size);
            self.niri.cursor_texture_cache.clear();
        }

        // We need &mut self to reload the xkb config, so just store it here.
        if config.input.keyboard.xkb != old_config.input.keyboard.xkb {
            reload_xkb = Some(config.input.keyboard.xkb.clone());
        }

        // Reload the repeat info.
        if config.input.keyboard.repeat_rate != old_config.input.keyboard.repeat_rate
            || config.input.keyboard.repeat_delay != old_config.input.keyboard.repeat_delay
        {
            let keyboard = self.niri.seat.get_keyboard().unwrap();
            keyboard.change_repeat_info(
                config.input.keyboard.repeat_rate.into(),
                config.input.keyboard.repeat_delay.into(),
            );
        }

        if config.input.mouse != old_config.input.mouse {
            libinput_config_changed = true;
        }

        if config.outputs == self.niri.config_file_output_config {
            // Output config did not change from the last disk load, so we need to preserve the
            // transient changes.
            preserved_output_config = Some(mem::take(&mut old_config.outputs));
        } else {
            output_config_changed = true;
            self.niri
                .config_file_output_config
                .clone_from(&config.outputs);
        }

        let binds_changed = config.binds != old_config.binds;
        let new_mod_key = self.backend.mod_key(&config);
        if new_mod_key != self.backend.mod_key(&old_config) || binds_changed {
            self.niri
                .hotkey_overlay
                .on_hotkey_config_updated(new_mod_key);
            self.niri.mods_with_mouse_binds = mods_with_mouse_binds(new_mod_key, &config.binds);
            self.niri.mods_with_wheel_binds = mods_with_wheel_binds(new_mod_key, &config.binds);
        }

        if config.window_rules != old_config.window_rules {
            window_rules_changed = true;
        }

        if config.layer_rules != old_config.layer_rules {
            layer_rules_changed = true;
        }

        if config.cursor.hide_after_inactive_ms != old_config.cursor.hide_after_inactive_ms {
            cursor_inactivity_timeout_changed = true;
        }

        // FIXME: move backdrop rendering into layout::Monitor, then this will become unnecessary.
        if config.overview.backdrop_color != old_config.overview.backdrop_color {
            output_config_changed = true;
        }
        if config.layout.background_color != old_config.layout.background_color {
            output_config_changed = true;
        }

        *old_config = config;

        if let Some(outputs) = preserved_output_config {
            old_config.outputs = outputs;
        }

        // Release the borrow.
        drop(old_config);

        // Now with a &mut self we can reload the xkb config.
        if let Some(mut xkb) = reload_xkb {
            let mut set_xkb_config = true;

            // It's fine to .take() the xkb file, as this is a
            // clone and the file field is not used in the XkbConfig.
            if let Some(xkb_file) = xkb.file.take() {
                if let Err(err) = self.set_xkb_file(xkb_file) {
                    warn!("error reloading xkb_file: {err:?}");
                } else {
                    // We successfully set xkb file so we don't need to fallback to XkbConfig.
                    set_xkb_config = false;
                }
            }

            if set_xkb_config {
                // If xkb is unset in the niri config, use settings from locale1.
                if xkb == Xkb::default() {
                    trace!("using xkb from locale1");
                    xkb = self.niri.xkb_from_locale1.clone().unwrap_or_default();
                }

                self.set_xkb_config(xkb.to_xkb_config());
            }

            self.ipc_keyboard_layouts_changed();
        }

        if libinput_config_changed {
            let config = self.niri.config.borrow();
            for mut device in self.niri.devices.iter().cloned() {
                apply_libinput_settings(&config.input, &mut device);
            }
        }

        if output_config_changed {
            self.reload_output_config();
        }

        if window_rules_changed {
            self.niri.recompute_window_rules();
        }

        if layer_rules_changed {
            self.niri.recompute_layer_rules();
        }

        if cursor_inactivity_timeout_changed {
            // Force reset due to timeout change.
            self.niri.pointer_inactivity_timer_got_reset = false;
            self.niri.reset_pointer_inactivity_timer();
        }

        // Can't really update xdg-decoration settings since we have to hide the globals for CSD
        // due to the SDL2 bug... I don't imagine clients are prepared for the xdg-decoration
        // global suddenly appearing? Either way, right now it's live-reloaded in a sense that new
        // clients will use the new xdg-decoration setting.

        self.niri.queue_redraw_all();
    }

    /// # Panics
    ///
    /// Panics if:
    /// - `output` has no `OutputName` in its user data, which niri always attaches when creating an output
    /// - `output` has no current mode set
    pub fn reload_output_config(&mut self) {
        let mut resized_outputs = vec![];
        let mut recolored_outputs = vec![];

        for output in self.niri.global_space.outputs() {
            let name = output.user_data().get::<OutputName>().unwrap();
            let full_config = self.niri.config.borrow_mut();
            let config = full_config.outputs.find(name);

            let scale = config.and_then(|c| c.scale).map_or_else(
                || {
                    let size_mm = output.physical_properties().size;
                    let resolution = output.current_mode().unwrap().size;
                    guess_monitor_scale(size_mm, resolution)
                },
                |s| s.0,
            );
            let scale = closest_representable_scale(scale.clamp(0.1, 10.));

            let transform = panel_orientation(output)
                + config.map_or(Transform::Normal, |c| ipc_transform_to_smithay(c.transform));

            // Both sides are passed through closest_representable_scale(), which snaps to a
            // deterministic, canonical value, so an exact comparison correctly detects "no
            // change" and avoids an unnecessary change_current_state() call below.
            #[allow(clippy::float_cmp)]
            let scale_changed = output.current_scale().fractional_scale() != scale;
            if scale_changed || output.current_transform() != transform {
                output.change_current_state(
                    None,
                    Some(transform),
                    Some(output::Scale::Fractional(scale)),
                    None,
                );
                self.niri.ipc_outputs_changed = true;
                resized_outputs.push(output.clone());
            }

            let mut backdrop_color = config
                .and_then(|c| c.backdrop_color)
                .unwrap_or(full_config.overview.backdrop_color)
                .to_array_unpremul();
            backdrop_color[3] = 1.;
            let backdrop_color = Color32F::from(backdrop_color);

            if let Some(state) = self.niri.output_state.get_mut(output)
                && state.backdrop_buffer.color() != backdrop_color
            {
                state.backdrop_buffer.set_color(backdrop_color);
                recolored_outputs.push(output.clone());
            }

            for mon in self.niri.layout.monitors_mut() {
                if mon.output() != output {
                    continue;
                }

                let mut layout_config = config.and_then(|c| c.layout.clone());
                // Support the deprecated non-layout background-color key.
                if let Some(layout) = &mut layout_config
                    && layout.background_color.is_none()
                {
                    layout.background_color = config.and_then(|c| c.background_color);
                }

                if mon.update_layout_config(layout_config) {
                    // Also redraw these; if anything, the background color could've changed.
                    recolored_outputs.push(output.clone());
                }
                break;
            }
        }

        for output in resized_outputs {
            self.niri.output_resized(&output);
        }

        for output in recolored_outputs {
            self.niri.queue_redraw(&output);
        }

        self.backend.on_output_config_changed(&mut self.niri);

        self.niri.reposition_outputs(None);
    }

    /// # Panics
    ///
    /// Panics if `output` has no `OutputName` in its user data, which niri always attaches when creating an output.
    pub fn modify_output_config<F>(&mut self, name: &str, fun: F)
    where
        F: FnOnce(&mut niri_config::Output),
    {
        // Try hard to find the output config section corresponding to the output set by the
        // user. Since if we add a new section and some existing section also matches the
        // output, then our new section won't do anything.
        let temp;
        let match_name = if let Some(output) = self.niri.output_by_name_match(name) {
            output.user_data().get::<OutputName>().unwrap()
        } else if let Some(output_name) = self
            .backend
            .tty_checked()
            .and_then(|tty| tty.disconnected_connector_name_by_name_match(name))
        {
            temp = output_name;
            &temp
        } else {
            // Even if name is "make model serial", matching will work fine this way.
            temp = OutputName {
                connector: name.to_owned(),
                make: None,
                model: None,
                serial: None,
            };
            &temp
        };

        let mut config = self.niri.config.borrow_mut();
        let config = if let Some(config) = config.outputs.find_mut(match_name) {
            config
        } else {
            config.outputs.0.push(niri_config::Output {
                // Save name as set by the user.
                name: String::from(name),
                ..Default::default()
            });
            config.outputs.0.last_mut().unwrap()
        };

        fun(config);
    }

    pub fn apply_transient_output_config(&mut self, name: &str, action: niri_ipc::OutputAction) {
        self.modify_output_config(name, move |config| match action {
            niri_ipc::OutputAction::Off => config.off = true,
            niri_ipc::OutputAction::On => config.off = false,
            niri_ipc::OutputAction::Mode { mode } => {
                config.mode = match mode {
                    niri_ipc::ModeToSet::Automatic => None,
                    niri_ipc::ModeToSet::Specific(mode) => Some(niri_config::output::Mode {
                        custom: false,
                        mode,
                    }),
                };
                config.modeline = None;
            }
            niri_ipc::OutputAction::CustomMode { mode } => {
                config.mode = Some(niri_config::output::Mode { custom: true, mode });
                config.modeline = None;
            }
            niri_ipc::OutputAction::Modeline {
                clock,
                hdisplay,
                hsync_start,
                hsync_end,
                htotal,
                vdisplay,
                vsync_start,
                vsync_end,
                vtotal,
                hsync_polarity,
                vsync_polarity,
            } => {
                // Do not reset config.mode to None since it's used as a fallback.
                config.modeline = Some(niri_config::output::Modeline {
                    clock,
                    hdisplay,
                    hsync_start,
                    hsync_end,
                    htotal,
                    vdisplay,
                    vsync_start,
                    vsync_end,
                    vtotal,
                    hsync_polarity,
                    vsync_polarity,
                });
            }
            niri_ipc::OutputAction::Scale { scale } => {
                config.scale = match scale {
                    niri_ipc::ScaleToSet::Automatic => None,
                    niri_ipc::ScaleToSet::Specific(scale) => Some(FloatOrInt(scale)),
                }
            }
            niri_ipc::OutputAction::Transform { transform } => config.transform = transform,
            niri_ipc::OutputAction::Position { position } => {
                config.position = match position {
                    niri_ipc::PositionToSet::Automatic => None,
                    niri_ipc::PositionToSet::Specific(position) => Some(niri_config::Position {
                        x: position.x,
                        y: position.y,
                    }),
                }
            }
            niri_ipc::OutputAction::MaxBpc { max_bpc } => config.max_bpc = Some(MaxBpc(max_bpc)),
        });

        self.reload_output_config();
    }

    /// # Panics
    ///
    /// Panics if an internal mutex is poisoned.
    pub fn refresh_ipc_outputs(&mut self) {
        if !self.niri.ipc_outputs_changed {
            return;
        }
        self.niri.ipc_outputs_changed = false;

        for ipc_output in self.backend.ipc_outputs().lock().unwrap().values_mut() {
            let logical = self
                .niri
                .global_space
                .outputs()
                .find(|output| output.name() == ipc_output.name)
                .map(logical_output);
            ipc_output.logical = logical;
        }
    }

    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn open_screenshot_ui(&mut self, show_pointer: bool, path: Option<String>) {
        if self.niri.is_locked() || self.niri.screenshot_ui.is_open() {
            return;
        }

        let default_output = self
            .niri
            .output_under_cursor()
            .or_else(|| self.niri.layout.active_output().cloned());
        let Some(default_output) = default_output else {
            return;
        };

        self.niri.update_render_elements(None);

        let Some(screenshots) = self
            .backend
            .with_primary_renderer(|renderer| self.niri.capture_screenshots(renderer).collect())
        else {
            return;
        };

        // Now that we captured the screenshots, clear grabs like drag-and-drop, etc.
        self.niri.seat.get_pointer().unwrap().unset_grab(
            self,
            SERIAL_COUNTER.next_serial(),
            wayland_time_now(),
        );

        self.backend.with_primary_renderer(|renderer| {
            self.niri
                .screenshot_ui
                .open(renderer, screenshots, default_output, show_pointer, path)
        });

        self.niri
            .cursor_manager
            .set_cursor_image(CursorImageStatus::Named(CursorIcon::Crosshair));
        self.niri.queue_redraw_all();
    }

    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn handle_pick_color(&mut self, tx: async_channel::Sender<Option<niri_ipc::PickedColor>>) {
        let pointer = self.niri.seat.get_pointer().unwrap();
        let start_data = PointerGrabStartData {
            focus: None,
            button: 0,
            location: pointer.current_location(),
        };
        let grab = PickColorGrab::new(start_data);
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
        self.niri.pick_color = Some(tx);
        self.niri
            .cursor_manager
            .set_cursor_image(CursorImageStatus::Named(CursorIcon::Crosshair));
        self.niri.queue_redraw_all();
    }

    pub fn confirm_screenshot(&mut self, write_to_disk: bool) {
        let ScreenshotUi::Open { path, .. } = &mut self.niri.screenshot_ui else {
            return;
        };
        let path = path.take();

        self.backend.with_primary_renderer(|renderer| {
            match self.niri.screenshot_ui.capture(renderer) {
                Ok((size, pixels)) => {
                    if let Err(err) = self.niri.save_screenshot(size, pixels, write_to_disk, path) {
                        warn!("error saving screenshot: {err:?}");
                    }
                }
                Err(err) => {
                    warn!("error capturing screenshot: {err:?}");
                }
            }
        });

        self.niri.screenshot_ui.close();
        self.niri
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());
        self.niri.queue_redraw_all();
    }

    pub fn store_unmap_snapshot(&mut self, window: &Window) {
        self.backend.with_primary_renderer(|renderer| {
            self.niri.layout.store_unmap_snapshot(renderer, window);
        });
    }
}

impl Niri {
    /// # Panics
    ///
    /// Panics if creating the async executor fails, which should not happen during normal startup.
    //
    // This constructor sequentially initializes ~30 compositor subsystems (event loop sources,
    // Wayland globals, seat/input, layout, IPC server, cursor manager, ...) and binds most of
    // them into the single `Niri` struct returned at the end; splitting it into helpers would
    // mean either returning partially-built state between calls or passing a long list of
    // already-constructed subsystems by value, neither of which reads more clearly than the
    // current linear build-up.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        config: Rc<RefCell<Config>>,
        event_loop: LoopHandle<'static, State>,
        stop_signal: LoopSignal,
        display: Display<State>,
        backend: &Backend,
        create_wayland_socket: bool,
    ) -> Self {
        fn client_is_unrestricted(client: &Client) -> bool {
            !client.get_data::<ClientState>().unwrap().restricted
        }

        let (executor, scheduler) = calloop::futures::executor().unwrap();
        event_loop.insert_source(executor, |(), (), _| ()).unwrap();

        let display_handle = display.handle();
        let config_ = config.borrow();
        let config_file_output_config = config_.outputs.clone();

        let mut animation_clock = Clock::default();

        let rate = 1.0 / config_.animations.slowdown.max(0.001);
        animation_clock.set_rate(rate);
        animation_clock.set_complete_instantly(config_.animations.off);

        let layout = Layout::new(animation_clock.clone(), &config_);

        let (blocker_cleared_tx, blocker_cleared_rx) = mpsc::channel();

        let compositor_state = CompositorState::new_v6::<State>(&display_handle);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<State>(
            &display_handle,
            [WmCapabilities::Fullscreen, WmCapabilities::Maximize],
        );
        let xdg_decoration_state =
            XdgDecorationState::new_with_filter::<State, _>(&display_handle, |client| {
                client
                    .get_data::<ClientState>()
                    .unwrap()
                    .can_view_decoration_globals
            });
        let kde_decoration_state = KdeDecorationState::new_with_filter::<State, _>(
            &display_handle,
            // If we want CSD we will hide the global.
            KdeDecorationsMode::Server,
            |client| {
                client
                    .get_data::<ClientState>()
                    .unwrap()
                    .can_view_decoration_globals
            },
        );
        let layer_shell_state = WlrLayerShellState::new_with_filter::<State, _>(
            &display_handle,
            client_is_unrestricted,
        );
        let session_lock_state =
            SessionLockManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        let shm_state = ShmState::new::<State>(
            &display_handle,
            vec![wl_shm::Format::Xbgr8888, wl_shm::Format::Abgr8888],
        );
        let output_manager_state =
            OutputManagerState::new_with_xdg_output::<State>(&display_handle);
        let dmabuf_state = DmabufState::new();
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<State>(&display_handle);
        let mut seat_state = SeatState::new();
        let relative_pointer_state = RelativePointerManagerState::new::<State>(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<State>(&display_handle);
        let data_device_state = DataDeviceState::new::<State>(&display_handle);
        let primary_selection_state =
            PrimarySelectionState::new_with_filter::<State, _>(&display_handle, |client| {
                !client
                    .get_data::<ClientState>()
                    .unwrap()
                    .primary_selection_disabled
            });
        let presentation_state =
            PresentationState::new::<State>(&display_handle, Monotonic::ID as u32);

        let viewporter_state = ViewporterState::new::<State>(&display_handle);
        let xdg_foreign_state = XdgForeignState::new::<State>(&display_handle);

        let mut seat: Seat<State> = seat_state.new_wl_seat(&display_handle, backend.seat_name());
        let keyboard = match seat.add_keyboard(
            config_.input.keyboard.xkb.to_xkb_config(),
            config_.input.keyboard.repeat_delay.into(),
            config_.input.keyboard.repeat_rate.into(),
        ) {
            Err(err) => {
                if matches!(err, smithay::input::keyboard::Error::BadKeymap) {
                    warn!("error loading the configured xkb keymap, trying default");
                } else {
                    warn!("error adding keyboard: {err:?}");
                }
                seat.add_keyboard(
                    XkbConfig::default(),
                    config_.input.keyboard.repeat_delay.into(),
                    config_.input.keyboard.repeat_rate.into(),
                )
                .unwrap()
            }
            Ok(keyboard) => keyboard,
        };
        if config_.input.keyboard.numlock {
            let mut modifier_state = keyboard.modifier_state();
            modifier_state.num_lock = true;
            keyboard.set_modifier_state(modifier_state);
        }
        seat.add_pointer();

        let cursor_shape_manager_state = CursorShapeManagerState::new::<State>(&display_handle);
        let cursor_manager =
            CursorManager::new(&config_.cursor.xcursor_theme, config_.cursor.xcursor_size);

        let mod_key = backend.mod_key(&config.borrow());
        let mods_with_mouse_binds = mods_with_mouse_binds(mod_key, &config_.binds);
        let mods_with_wheel_binds = mods_with_wheel_binds(mod_key, &config_.binds);

        let screenshot_ui = ScreenshotUi::new(animation_clock.clone(), config.clone());
        let config_error_notification =
            ConfigErrorNotification::new(animation_clock.clone(), config.clone());

        let mut hotkey_overlay = HotkeyOverlay::new(config.clone(), mod_key);
        if !config_.hotkey_overlay.skip_at_startup {
            hotkey_overlay.show();
        }

        let exit_confirm_dialog = ExitConfirmDialog::new(animation_clock.clone(), config.clone());

        event_loop
            .insert_source(
                Timer::from_duration(Duration::from_secs(1)),
                |_, (), state| {
                    state.niri.send_frame_callbacks_on_fallback_timer();
                    TimeoutAction::ToDuration(Duration::from_secs(1))
                },
            )
            .unwrap();

        let socket_name = create_wayland_socket.then(|| {
            let socket_source = ListeningSocketSource::new_auto().unwrap();
            let socket_name = socket_source.socket_name().to_os_string();
            event_loop
                .insert_source(socket_source, move |client, (), state| {
                    state.niri.insert_client(NewClient {
                        client,
                        restricted: false,
                        credentials_unknown: false,
                    });
                })
                .unwrap();
            socket_name
        });

        let ipc_server = match IpcServer::start(&event_loop, socket_name.as_deref()) {
            Ok(server) => Some(server),
            Err(err) => {
                warn!("error starting IPC server: {err:?}");
                None
            }
        };

        let display_source = Generic::new(display, Interest::READ, Mode::Level);
        event_loop
            .insert_source(display_source, |_, display, state| {
                // SAFETY: we don't drop the display.
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            })
            .unwrap();

        event_loop
            .insert_source(
                Timer::from_duration(Duration::from_mins(1)),
                |_, (), state| {
                    state.niri.is_at_startup = false;
                    state.niri.recompute_window_rules();
                    state.niri.recompute_layer_rules();
                    TimeoutAction::Drop
                },
            )
            .unwrap();

        drop(config_);
        let mut niri = Self {
            config,
            config_file_output_config,
            config_file_watcher: None,

            event_loop,
            scheduler,
            stop_signal,
            socket_name,
            display_handle,
            start_time: Instant::now(),
            is_at_startup: true,
            clock: animation_clock,

            layout,
            global_space: Space::default(),
            sorted_outputs: Vec::default(),
            output_state: HashMap::new(),
            unmapped_windows: HashMap::new(),
            unmapped_layer_surfaces: HashSet::new(),
            mapped_layer_surfaces: HashMap::new(),
            root_surface: HashMap::new(),
            dmabuf_pre_commit_hook: HashMap::new(),
            blocker_cleared_tx,
            blocker_cleared_rx,
            monitors_active: true,

            devices: HashSet::new(),

            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
            layer_shell_state,
            session_lock_state,
            viewporter_state,
            xdg_foreign_state,
            shm_state,
            output_manager_state,
            dmabuf_state,
            fractional_scale_manager_state,
            seat_state,
            relative_pointer_state,
            pointer_constraints_state,
            data_device_state,
            primary_selection_state,
            popups: PopupManager::default(),
            popup_grab: None,
            suppressed_keys: HashSet::new(),
            suppressed_buttons: HashSet::new(),
            bind_cooldown_timers: HashMap::new(),
            bind_repeat_timer: Option::default(),
            presentation_state,

            seat,
            keyboard_focus: KeyboardFocus::Layout { surface: None },
            layer_shell_on_demand_focus: None,
            xkb_from_locale1: None,
            cursor_manager,
            cursor_texture_cache: CursorTextureCache::default(),
            cursor_shape_manager_state,
            dnd_icon: None,
            pointer_contents: PointContents::default(),
            pointer_visibility: PointerVisibility::Visible,
            pointer_inactivity_timer: None,
            pointer_inactivity_timer_got_reset: false,
            notified_activity_this_iteration: false,
            pointer_inside_hot_corner: false,
            vertical_wheel_tracker: ScrollTracker::new(120),
            horizontal_wheel_tracker: ScrollTracker::new(120),
            mods_with_mouse_binds,
            mods_with_wheel_binds,

            lock_state: LockState::Unlocked,

            screenshot_ui,
            config_error_notification,
            hotkey_overlay,
            exit_confirm_dialog,

            pick_window: None,
            pick_color: None,

            ipc_server,
            ipc_outputs_changed: false,
        };

        niri.reset_pointer_inactivity_timer();

        niri
    }

    pub fn insert_client(&mut self, client: NewClient) {
        let NewClient {
            client,
            restricted,
            credentials_unknown,
        } = client;

        let config = self.config.borrow();
        let data = Arc::new(ClientState {
            compositor_state: CompositorClientState::default(),
            can_view_decoration_globals: config.prefer_no_csd,
            primary_selection_disabled: config.clipboard.disable_primary,
            restricted,
            credentials_unknown,
        });

        if let Err(err) = self.display_handle.insert_client(client, data) {
            warn!("error inserting client: {err}");
        }
    }

    /// Repositions all outputs, optionally adding a new output.
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - `output` has no `OutputName` in its user data, which niri always attaches when creating an output
    /// - The output has no known geometry in the global space
    pub fn reposition_outputs(&mut self, new_output: Option<&Output>) {
        #[derive(Debug)]
        struct Data {
            output: Output,
            name: OutputName,
            position: Option<Point<i32, Logical>>,
            config: Option<niri_config::Position>,
        }

        let config = self.config.borrow();
        let mut outputs = vec![];
        for output in self.global_space.outputs().chain(new_output) {
            let name = output.user_data().get::<OutputName>().unwrap();
            let position = self.global_space.output_geometry(output).map(|geo| geo.loc);
            let config = config.outputs.find(name).and_then(|c| c.position);

            outputs.push(Data {
                output: output.clone(),
                name: name.clone(),
                position,
                config,
            });
        }
        drop(config);

        for Data { output, .. } in &outputs {
            self.global_space.unmap_output(output);
        }

        // Connectors can appear in udev in any order. If we sort by name then we get output
        // positioning that does not depend on the order they appeared.
        //
        // This sorting first compares by make/model/serial so that it is stable regardless of the
        // connector name. However, if make/model/serial is equal or unknown, then it does fall
        // back to comparing the connector name, which should always be unique.
        outputs.sort_unstable_by(|a, b| a.name.compare(&b.name));

        // Place all outputs with explicitly configured position first, then the unconfigured ones.
        outputs.sort_by_key(|d| d.config.is_none());

        trace!(
            "placing outputs in order: {:?}",
            outputs.iter().map(|d| &d.name.connector)
        );

        self.sorted_outputs = outputs
            .iter()
            .map(|Data { output, .. }| output.clone())
            .collect();

        for data in outputs {
            let Data {
                output,
                name,
                position,
                config,
            } = data;

            let size = output_size(&output).to_i32_round();

            let new_position = config
                .map(|pos| Point::from((pos.x, pos.y)))
                .filter(|pos| {
                    // Ensure that the requested position does not overlap any existing output.
                    let target_geom = Rectangle::new(*pos, size);

                    let overlap = self
                        .global_space
                        .outputs()
                        .map(|output| self.global_space.output_geometry(output).unwrap())
                        .find(|geom| geom.overlaps(target_geom));

                    if let Some(overlap) = overlap {
                        warn!(
                            "output {} at x={} y={} sized {}x{} \
                             overlaps an existing output at x={} y={} sized {}x{}, \
                             falling back to automatic placement",
                            name.connector,
                            pos.x,
                            pos.y,
                            size.w,
                            size.h,
                            overlap.loc.x,
                            overlap.loc.y,
                            overlap.size.w,
                            overlap.size.h,
                        );

                        false
                    } else {
                        true
                    }
                })
                .unwrap_or_else(|| {
                    let x = self
                        .global_space
                        .outputs()
                        .map(|output| self.global_space.output_geometry(output).unwrap())
                        .map(|geom| geom.loc.x + geom.size.w)
                        .max()
                        .unwrap_or(0);

                    Point::from((x, 0))
                });

            self.global_space.map_output(&output, new_position);

            // By passing new_output as an Option, rather than mapping it into a bogus location
            // in global_space, we ensure that this branch always runs for it.
            if Some(new_position) != position {
                debug!(
                    "putting output {} at x={} y={}",
                    name.connector, new_position.x, new_position.y
                );
                output.change_current_state(None, None, None, Some(new_position));
                self.ipc_outputs_changed = true;
                self.queue_redraw(&output);
            }
        }
    }

    /// # Panics
    ///
    /// Panics if:
    /// - `output` has no `OutputName` in its user data, which niri always attaches when creating an output
    /// - `output` has no current mode set
    //
    // Taking `output` by value here (even though the body only ever clones or borrows it)
    // matches its two call sites in src/backend/headless.rs and src/backend/tty.rs, one of
    // which already owns the `Output` outright; switching to `&Output` would only move the
    // clone from inside this function to the tty.rs call site instead of removing it.
    #[allow(clippy::needless_pass_by_value)]
    pub fn add_output(&mut self, output: Output, refresh_interval: Option<Duration>) {
        let global = output.create_global::<State>(&self.display_handle);

        let name = output.user_data().get::<OutputName>().unwrap();

        let config = self.config.borrow();
        let c = config.outputs.find(name);
        let scale = c.and_then(|c| c.scale).map_or_else(
            || {
                let size_mm = output.physical_properties().size;
                let resolution = output.current_mode().unwrap().size;
                guess_monitor_scale(size_mm, resolution)
            },
            |s| s.0,
        );
        let scale = closest_representable_scale(scale.clamp(0.1, 10.));

        let transform = panel_orientation(&output)
            + c.map_or(Transform::Normal, |c| ipc_transform_to_smithay(c.transform));

        let mut backdrop_color = c
            .and_then(|c| c.backdrop_color)
            .unwrap_or(config.overview.backdrop_color)
            .to_array_unpremul();
        backdrop_color[3] = 1.;

        let mut layout_config = c.and_then(|c| c.layout.clone());
        // Support the deprecated non-layout background-color key.
        if let Some(layout) = &mut layout_config
            && layout.background_color.is_none()
        {
            layout.background_color = c.and_then(|c| c.background_color);
        }
        drop(config);

        // Set scale and transform before adding to the layout since that will read the output size.
        output.change_current_state(
            None,
            Some(transform),
            Some(output::Scale::Fractional(scale)),
            None,
        );

        self.layout.add_output(output.clone(), layout_config);

        let lock_render_state = if self.is_locked() {
            // We haven't rendered anything yet so it's as good as locked.
            LockRenderState::Locked
        } else {
            LockRenderState::Unlocked
        };

        let size = output_size(&output);
        let state = OutputState {
            global,
            redraw_state: RedrawState::Idle,
            unfinished_animations_remain: false,
            frame_clock: FrameClock::new(refresh_interval),
            vblank_throttle: VBlankThrottle::new(self.event_loop.clone(), name.connector.clone()),
            frame_callback_sequence: 0,
            backdrop_buffer: SolidColorBuffer::new(size, backdrop_color),
            lock_render_state,
            lock_surface: None,
            lock_color_buffer: SolidColorBuffer::new(size, CLEAR_COLOR_LOCKED),
            screen_transition: None,
        };
        let rv = self.output_state.insert(output.clone(), state);
        assert!(rv.is_none(), "output was already tracked");

        // Must be last since it will call queue_redraw(output) which needs things to be filled-in.
        self.reposition_outputs(Some(&output));
    }

    pub fn output_exists(&self, output: &Output) -> bool {
        self.output_state.contains_key(output)
    }

    /// Converts a `WlOutput` to a corresponding `Output` if it exists.
    ///
    /// Compared to raw `Output::from_resource`, this method also verifies that the output still
    /// exists in niri. Right after the output global is disabled, but before it is removed for
    /// good, `Output::from_resource` will succeed, but since niri already forgot the output,
    /// accessing it can cause logic bugs.
    pub fn output_from_resource(&self, wl_output: &WlOutput) -> Option<Output> {
        Output::from_resource(wl_output).filter(|output| self.output_exists(output))
    }

    /// # Panics
    ///
    /// Panics if `output` is not present in `output_state`.
    pub fn remove_output(&mut self, output: &Output) {
        for layer in layer_map_for_output(output).layers() {
            layer.layer_surface().send_close();
        }

        self.layout.remove_output(output);
        self.global_space.unmap_output(output);
        self.reposition_outputs(None);

        let state = self.output_state.remove(output).unwrap();

        match state.redraw_state {
            RedrawState::Idle | RedrawState::Queued | RedrawState::WaitingForVBlank { .. } => (),
            RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                self.event_loop.remove(token);
            }
        }

        // Disable the output global and remove some time later to give the clients some time to
        // process it.
        let global = state.global;
        self.display_handle.disable_global::<State>(global.clone());
        self.event_loop
            .insert_source(
                Timer::from_duration(Duration::from_secs(10)),
                move |_, (), state| {
                    state
                        .niri
                        .display_handle
                        .remove_global::<State>(global.clone());
                    TimeoutAction::Drop
                },
            )
            .unwrap();

        match mem::take(&mut self.lock_state) {
            LockState::Locking(confirmation) => {
                // We're locking and an output was removed, check if the requirements are now met.
                let all_locked = self
                    .output_state
                    .values()
                    .all(|state| state.lock_render_state == LockRenderState::Locked);

                if all_locked {
                    let lock = confirmation.ext_session_lock().clone();
                    confirmation.lock();
                    self.lock_state = LockState::Locked(lock);
                } else {
                    // Still waiting.
                    self.lock_state = LockState::Locking(confirmation);
                }
            }
            lock_state => {
                self.lock_state = lock_state;
                self.maybe_continue_to_locking();
            }
        }

        if self.screenshot_ui.close() {
            self.cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());
            self.queue_redraw_all();
        }
    }

    /// # Panics
    ///
    /// Panics if `output` has no current mode set.
    pub fn output_resized(&mut self, output: &Output) {
        let output_size = output_size(output);
        let scale = output.current_scale();
        let transform = output.current_transform();

        {
            let mut layer_map = layer_map_for_output(output);
            for layer in layer_map.layers() {
                layer.with_surfaces(|surface, data| {
                    send_scale_transform(surface, data, scale, transform);
                });

                if let Some(mapped) = self.mapped_layer_surfaces.get_mut(layer) {
                    mapped.update_sizes(output_size, scale.fractional_scale());
                }
            }
            layer_map.arrange();
        }

        self.layout.update_output_size(output);

        if let Some(state) = self.output_state.get_mut(output) {
            state.backdrop_buffer.resize(output_size);

            state.lock_color_buffer.resize(output_size);
            if let Some(lock_surface) = &state.lock_surface {
                configure_lock_surface(lock_surface, output);
            }
        }

        // If the output size changed with an open screenshot UI, close the screenshot UI.
        if let Some((old_size, old_scale, old_transform)) = self.screenshot_ui.output_size(output) {
            let output_mode = output.current_mode().unwrap();
            let size = transform.transform_size(output_mode.size);
            let scale = output.current_scale().fractional_scale();
            // FIXME: scale changes and transform flips shouldn't matter but they currently do since
            // I haven't quite figured out how to draw the screenshot textures in
            // physical coordinates.
            #[allow(
                clippy::float_cmp,
                reason = "detecting any scale change at all, not comparing computed values"
            )]
            let scale_or_geometry_changed =
                old_size != size || old_scale != scale || old_transform != transform;
            if scale_or_geometry_changed {
                self.screenshot_ui.close();
                self.cursor_manager
                    .set_cursor_image(CursorImageStatus::default_named());
                self.queue_redraw_all();
                return;
            }
        }

        self.queue_redraw(output);
    }

    pub fn deactivate_monitors(&mut self, backend: &mut Backend) {
        if !self.monitors_active {
            return;
        }

        self.monitors_active = false;
        backend.set_monitors_active(false);
    }

    pub fn activate_monitors(&mut self, backend: &mut Backend) {
        if self.monitors_active {
            return;
        }

        self.monitors_active = true;
        backend.set_monitors_active(true);

        self.queue_redraw_all();
    }

    /// # Panics
    ///
    /// Panics if an internal invariant is violated.
    pub fn output_under(&self, pos: Point<f64, Logical>) -> Option<(&Output, Point<f64, Logical>)> {
        let output = self.global_space.output_under(pos).next()?;
        let pos_within_output = pos
            - self
                .global_space
                .output_geometry(output)
                .unwrap()
                .loc
                .to_f64();

        Some((output, pos_within_output))
    }

    fn is_inside_hot_corner(&self, output: &Output, pos: Point<f64, Logical>) -> bool {
        let config = self.config.borrow();
        let hot_corners = output
            .user_data()
            .get::<OutputName>()
            .and_then(|name| config.outputs.find(name))
            .and_then(|c| c.hot_corners)
            .unwrap_or(config.gestures.hot_corners);

        if hot_corners.off {
            return false;
        }

        // Use size from the ceiled output geometry, since that's what we currently use for pointer
        // motion clamping.
        let geom = self.global_space.output_geometry(output).unwrap();
        let size = geom.size.to_f64();

        let contains = move |corner: Point<f64, Logical>| {
            Rectangle::new(corner, Size::new(1., 1.)).contains(pos)
        };

        if hot_corners.top_right && contains(Point::new(size.w - 1., 0.)) {
            return true;
        }
        if hot_corners.bottom_left && contains(Point::new(0., size.h - 1.)) {
            return true;
        }
        if hot_corners.bottom_right && contains(Point::new(size.w - 1., size.h - 1.)) {
            return true;
        }

        // If the user didn't explicitly set any corners, we default to top-left.
        if (hot_corners.top_left
            || !(hot_corners.top_right || hot_corners.bottom_right || hot_corners.bottom_left))
            && contains(Point::new(0., 0.))
        {
            return true;
        }

        false
    }

    /// # Panics
    ///
    /// Panics if:
    /// - `output` has no associated monitor in the layout
    /// - The layer surface has no computed geometry yet
    pub fn is_sticky_obscured_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        // The ordering here must be consistent with the ordering in render() so that input is
        // consistent with the visuals.

        // Check if some layer-shell surface is on top.
        let layers = layer_map_for_output(output);
        let layer_surface_under = |layer, popup| {
            layers
                .layers_on(layer)
                .rev()
                .find_map(|layer| {
                    let mapped = self.mapped_layer_surfaces.get(layer)?;

                    let mut layer_pos_within_output =
                        layers.layer_geometry(layer).unwrap().loc.to_f64();
                    layer_pos_within_output += mapped.bob_offset();

                    let surface_type = if popup {
                        WindowSurfaceType::POPUP
                    } else {
                        WindowSurfaceType::TOPLEVEL
                    } | WindowSurfaceType::SUBSURFACE;
                    layer.surface_under(pos_within_output - layer_pos_within_output, surface_type)
                })
                .is_some()
        };

        let layer_toplevel_under = |layer| layer_surface_under(layer, false);
        let layer_popup_under = |layer| layer_surface_under(layer, true);

        if layer_popup_under(Layer::Overlay) || layer_toplevel_under(Layer::Overlay) {
            return true;
        }

        let mon = self.layout.monitor_for_output(output).unwrap();
        if mon.render_above_top_layer() {
            return false;
        }

        if self.is_inside_hot_corner(output, pos_within_output) {
            return true;
        }

        if layer_popup_under(Layer::Top) || layer_toplevel_under(Layer::Top) {
            return true;
        }

        false
    }

    /// # Panics
    ///
    /// Panics if the layer surface has no computed geometry yet.
    pub fn is_layout_obscured_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        if self.layout.is_overview_open() {
            return false;
        }

        // Check if some layer-shell surface is on top.
        let layers = layer_map_for_output(output);
        let layer_popup_under = |layer| {
            layers
                .layers_on(layer)
                .rev()
                .find_map(|layer_surface| {
                    let mapped = self.mapped_layer_surfaces.get(layer_surface)?;
                    if mapped.place_within_backdrop() {
                        return None;
                    }

                    let mut layer_pos_within_output =
                        layers.layer_geometry(layer_surface).unwrap().loc.to_f64();
                    layer_pos_within_output += mapped.bob_offset();

                    // Background and bottom layers move together with the workspaces.
                    let mon = self.layout.monitor_for_output(output)?;
                    let (_, geo) = mon.workspace_under(pos_within_output)?;
                    layer_pos_within_output += geo.loc;

                    let surface_type = WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE;
                    layer_surface
                        .surface_under(pos_within_output - layer_pos_within_output, surface_type)
                })
                .is_some()
        };

        if layer_popup_under(Layer::Bottom) || layer_popup_under(Layer::Background) {
            return true;
        }

        false
    }

    /// Returns the workspace under the position to be activated.
    ///
    /// The return value is an output and a workspace index on it.
    pub fn workspace_under(
        &self,
        extended_bounds: bool,
        pos: Point<f64, Logical>,
    ) -> Option<(Output, &Workspace<Mapped>)> {
        if self.exit_confirm_dialog.is_open() || self.is_locked() || self.screenshot_ui.is_open() {
            return None;
        }

        let (output, pos_within_output) = self.output_under(pos)?;

        if self.is_sticky_obscured_under(output, pos_within_output) {
            return None;
        }

        if self.is_layout_obscured_under(output, pos_within_output) {
            return None;
        }

        let ws = self
            .layout
            .workspace_under(extended_bounds, output, pos_within_output)?;
        Some((output.clone(), ws))
    }

    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn workspace_under_cursor(
        &self,
        extended_bounds: bool,
    ) -> Option<(Output, &Workspace<Mapped>)> {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.workspace_under(extended_bounds, pos)
    }

    /// Returns the window under the position to be activated.
    ///
    /// The cursor may be inside the window's activation region, but not within the window's input
    /// region.
    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<&Mapped> {
        if self.exit_confirm_dialog.is_open() || self.is_locked() || self.screenshot_ui.is_open() {
            return None;
        }

        let (output, pos_within_output) = self.output_under(pos)?;

        if self.is_sticky_obscured_under(output, pos_within_output) {
            return None;
        }

        if let Some((window, _loc)) = self
            .layout
            .interactive_moved_window_under(output, pos_within_output)
        {
            return Some(window);
        }

        if self.is_layout_obscured_under(output, pos_within_output) {
            return None;
        }

        let (window, _loc) = self.layout.window_under(output, pos_within_output)?;
        Some(window)
    }

    /// Returns the window under the cursor to be activated.
    ///
    /// The cursor may be inside the window's activation region, but not within the window's input
    /// region.
    ///
    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn window_under_cursor(&self) -> Option<&Mapped> {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.window_under(pos)
    }

    /// Returns contents under the given point.
    ///
    /// We don't have a proper global space for all windows, so this function converts window
    /// locations to global space according to where they are rendered.
    ///
    /// This function does not take pointer or touch grabs into account.
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - The output has no known geometry in the global space
    /// - `output` has no associated monitor in the layout
    /// - The layer surface has no computed geometry yet
    // The function's own comment below states this ordering must stay consistent with
    // render()'s z-order (exit-confirm dialog, lock screen, layer-shell layers, layout,
    // ...); splitting it into helper functions would make it easy for that ordering to drift
    // out of sync between the two functions without either compiling error surfacing it.
    #[allow(clippy::too_many_lines)]
    pub fn contents_under(&self, pos: Point<f64, Logical>) -> PointContents {
        let mut rv = PointContents::default();

        let Some((output, pos_within_output)) = self.output_under(pos) else {
            return rv;
        };
        rv.output = Some(output.clone());
        let output_pos_in_global_space = self.global_space.output_geometry(output).unwrap().loc;

        // The ordering here must be consistent with the ordering in render() so that input is
        // consistent with the visuals.

        if self.exit_confirm_dialog.is_open() {
            return rv;
        } else if self.is_locked() {
            let Some(state) = self.output_state.get(output) else {
                return rv;
            };
            let Some(surface) = state.lock_surface.as_ref() else {
                return rv;
            };

            rv.surface = under_from_surface_tree(
                surface.wl_surface(),
                pos_within_output,
                // We put lock surfaces at (0, 0).
                (0, 0),
                WindowSurfaceType::ALL,
            )
            .map(|(surface, pos_within_output)| {
                (
                    surface,
                    (pos_within_output + output_pos_in_global_space).to_f64(),
                )
            });

            return rv;
        }

        if self.screenshot_ui.is_open() {
            return rv;
        }

        let layers = layer_map_for_output(output);
        let layer_surface_under = |layer, popup| {
            layers
                .layers_on(layer)
                .rev()
                .find_map(|layer_surface| {
                    let mapped = self.mapped_layer_surfaces.get(layer_surface)?;
                    if mapped.place_within_backdrop() {
                        return None;
                    }

                    let mut layer_pos_within_output =
                        layers.layer_geometry(layer_surface).unwrap().loc.to_f64();
                    layer_pos_within_output += mapped.bob_offset();

                    // Background and bottom layers move together with the workspaces.
                    if matches!(layer, Layer::Background | Layer::Bottom) {
                        let mon = self.layout.monitor_for_output(output)?;
                        let (_, geo) = mon.workspace_under(pos_within_output)?;
                        layer_pos_within_output += geo.loc;
                        // Don't need to deal with zoom here because in the overview background and
                        // bottom layers don't receive input.
                    }

                    let surface_type = if popup {
                        WindowSurfaceType::POPUP
                    } else {
                        WindowSurfaceType::TOPLEVEL
                    } | WindowSurfaceType::SUBSURFACE;

                    layer_surface
                        .surface_under(pos_within_output - layer_pos_within_output, surface_type)
                        .map(|(surface, pos_within_layer)| {
                            (
                                (surface, pos_within_layer.to_f64() + layer_pos_within_output),
                                layer_surface,
                            )
                        })
                })
                .map(|(s, l)| (Some(s), (None, Some(l.clone()))))
        };

        let layer_toplevel_under = |layer| layer_surface_under(layer, false);
        let layer_popup_under = |layer| layer_surface_under(layer, true);

        let mapped_hit_data = |(mapped, hit): (&Mapped, HitType)| {
            let window = &mapped.window;
            let surface_and_pos = if let HitType::Input { win_pos } = hit {
                let win_pos_within_output = win_pos;
                window
                    .surface_under(
                        pos_within_output - win_pos_within_output,
                        WindowSurfaceType::ALL,
                    )
                    .map(|(s, pos_within_window)| {
                        (s, pos_within_window.to_f64() + win_pos_within_output)
                    })
            } else {
                None
            };
            (surface_and_pos, (Some((window.clone(), hit)), None))
        };

        let interactive_moved_window_under = || {
            self.layout
                .interactive_moved_window_under(output, pos_within_output)
                .map(mapped_hit_data)
        };
        let window_under = || {
            self.layout
                .window_under(output, pos_within_output)
                .map(mapped_hit_data)
        };

        let mon = self.layout.monitor_for_output(output).unwrap();

        let mut under =
            layer_popup_under(Layer::Overlay).or_else(|| layer_toplevel_under(Layer::Overlay));

        let is_overview_open = self.layout.is_overview_open();

        // When rendering above the top layer, we put the regular monitor elements first.
        // Otherwise, we will render all layer-shell pop-ups and the top layer on top.
        if mon.render_above_top_layer() {
            under = under
                .or_else(interactive_moved_window_under)
                .or_else(window_under)
                .or_else(|| layer_popup_under(Layer::Top))
                .or_else(|| layer_toplevel_under(Layer::Top))
                .or_else(|| layer_popup_under(Layer::Bottom))
                .or_else(|| layer_popup_under(Layer::Background))
                .or_else(|| layer_toplevel_under(Layer::Bottom))
                .or_else(|| layer_toplevel_under(Layer::Background));
        } else {
            if self.is_inside_hot_corner(output, pos_within_output) {
                rv.hot_corner = true;
                return rv;
            }

            under = under
                .or_else(|| layer_popup_under(Layer::Top))
                .or_else(|| layer_toplevel_under(Layer::Top));

            under = under.or_else(interactive_moved_window_under);

            if !is_overview_open {
                under = under
                    .or_else(|| layer_popup_under(Layer::Bottom))
                    .or_else(|| layer_popup_under(Layer::Background));
            }

            under = under.or_else(window_under);

            if !is_overview_open {
                under = under
                    .or_else(|| layer_toplevel_under(Layer::Bottom))
                    .or_else(|| layer_toplevel_under(Layer::Background));
            }
        }

        let Some((mut surface_and_pos, (window, layer))) = under else {
            return rv;
        };

        if let Some((_, surface_pos)) = &mut surface_and_pos {
            *surface_pos += output_pos_in_global_space.to_f64();
        }

        rv.surface = surface_and_pos;
        rv.window = window;
        rv.layer = layer;
        rv
    }

    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn output_under_cursor(&self) -> Option<Output> {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.global_space.output_under(pos).next().cloned()
    }

    /// # Panics
    ///
    /// Panics if the output has no known geometry in the global space.
    pub fn output_left_of(&self, current: &Output) -> Option<Output> {
        let current_geo = self.global_space.output_geometry(current)?;
        let extended_geo = Rectangle::new(
            Point::from((i32::MIN / 2, current_geo.loc.y)),
            Size::from((i32::MAX, current_geo.size.h)),
        );

        self.global_space
            .outputs()
            .map(|output| (output, self.global_space.output_geometry(output).unwrap()))
            .filter(|(_, geo)| center(*geo).x < center(current_geo).x && geo.overlaps(extended_geo))
            .min_by_key(|(_, geo)| center(current_geo).x - center(*geo).x)
            .map(|(output, _)| output)
            .cloned()
    }

    /// # Panics
    ///
    /// Panics if the output has no known geometry in the global space.
    pub fn output_right_of(&self, current: &Output) -> Option<Output> {
        let current_geo = self.global_space.output_geometry(current)?;
        let extended_geo = Rectangle::new(
            Point::from((i32::MIN / 2, current_geo.loc.y)),
            Size::from((i32::MAX, current_geo.size.h)),
        );

        self.global_space
            .outputs()
            .map(|output| (output, self.global_space.output_geometry(output).unwrap()))
            .filter(|(_, geo)| center(*geo).x > center(current_geo).x && geo.overlaps(extended_geo))
            .min_by_key(|(_, geo)| center(*geo).x - center(current_geo).x)
            .map(|(output, _)| output)
            .cloned()
    }

    /// # Panics
    ///
    /// Panics if the output has no known geometry in the global space.
    pub fn output_up_of(&self, current: &Output) -> Option<Output> {
        let current_geo = self.global_space.output_geometry(current)?;
        let extended_geo = Rectangle::new(
            Point::from((current_geo.loc.x, i32::MIN / 2)),
            Size::from((current_geo.size.w, i32::MAX)),
        );

        self.global_space
            .outputs()
            .map(|output| (output, self.global_space.output_geometry(output).unwrap()))
            .filter(|(_, geo)| center(*geo).y < center(current_geo).y && geo.overlaps(extended_geo))
            .min_by_key(|(_, geo)| center(current_geo).y - center(*geo).y)
            .map(|(output, _)| output)
            .cloned()
    }

    /// # Panics
    ///
    /// Panics if the output has no known geometry in the global space.
    pub fn output_down_of(&self, current: &Output) -> Option<Output> {
        let current_geo = self.global_space.output_geometry(current)?;
        let extended_geo = Rectangle::new(
            Point::from((current_geo.loc.x, i32::MIN / 2)),
            Size::from((current_geo.size.w, i32::MAX)),
        );

        self.global_space
            .outputs()
            .map(|output| (output, self.global_space.output_geometry(output).unwrap()))
            .filter(|(_, geo)| center(*geo).y > center(current_geo).y && geo.overlaps(extended_geo))
            .min_by_key(|(_, geo)| center(*geo).y - center(current_geo).y)
            .map(|(output, _)| output)
            .cloned()
    }

    pub fn output_previous_of(&self, current: &Output) -> Option<Output> {
        self.sorted_outputs
            .iter()
            .rev()
            .skip_while(|&output| output != current)
            .nth(1)
            .or_else(|| self.sorted_outputs.last())
            .filter(|&output| output != current)
            .cloned()
    }

    pub fn output_next_of(&self, current: &Output) -> Option<Output> {
        self.sorted_outputs
            .iter()
            .skip_while(|&output| output != current)
            .nth(1)
            .or_else(|| self.sorted_outputs.first())
            .filter(|&output| output != current)
            .cloned()
    }

    pub fn output_left(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_left_of(active)
    }

    pub fn output_right(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_right_of(active)
    }

    pub fn output_up(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_up_of(active)
    }

    pub fn output_down(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_down_of(active)
    }

    pub fn output_previous(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_previous_of(active)
    }

    pub fn output_next(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_next_of(active)
    }

    pub fn find_output_and_workspace_index(
        &self,
        workspace_reference: WorkspaceReference,
    ) -> Option<(Option<Output>, usize)> {
        let (target_workspace_index, target_workspace) = match workspace_reference {
            WorkspaceReference::Index(index) => {
                return Some((None, index.saturating_sub(1) as usize));
            }
            WorkspaceReference::Name(name) => self.layout.find_workspace_by_name(&name)?,
            WorkspaceReference::Id(id) => {
                let id = WorkspaceId::specific(id);
                self.layout.find_workspace_by_id(id)?
            }
        };

        let target_output = target_workspace.current_output();
        Some((target_output.cloned(), target_workspace_index))
    }

    pub fn output_by_name_match(&self, target: &str) -> Option<&Output> {
        self.global_space
            .outputs()
            .find(|output| output_matches_name(output, target))
    }

    pub fn output_for_root(&self, root: &WlSurface) -> Option<&Output> {
        // Check the main layout.
        let win_out = self.layout.find_window_and_output(root);
        let layout_output = win_out.map(|(_, output)| output);
        if let Some(output) = layout_output {
            return output;
        }

        // Check layer-shell.
        let has_layer_surface = |o: &&Output| {
            layer_map_for_output(o)
                .layer_for_surface(root, WindowSurfaceType::TOPLEVEL)
                .is_some()
        };
        self.layout.outputs().find(has_layer_surface)
    }

    pub fn lock_surface_focus(&self) -> Option<WlSurface> {
        let output_under_cursor = self.output_under_cursor();
        let output = output_under_cursor
            .as_ref()
            .or_else(|| self.layout.active_output())
            .or_else(|| self.global_space.outputs().next())?;

        let state = self.output_state.get(output)?;
        state
            .lock_surface
            .as_ref()
            .map(LockSurface::wl_surface)
            .cloned()
    }

    /// Schedules an immediate redraw on all outputs if one is not already scheduled.
    pub fn queue_redraw_all(&mut self) {
        for state in self.output_state.values_mut() {
            state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
        }
    }

    /// Schedules an immediate redraw if one is not already scheduled.
    ///
    /// # Panics
    ///
    /// Panics if `output` is not present in `output_state`.
    pub fn queue_redraw(&mut self, output: &Output) {
        let state = self.output_state.get_mut(output).unwrap();
        state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
    }

    pub fn redraw_queued_outputs(&mut self, backend: &mut Backend) {
        while let Some((output, _)) = self.output_state.iter().find(|(_, state)| {
            matches!(
                state.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            )
        }) {
            trace!("redrawing output");
            let output = output.clone();
            self.redraw(backend, &output);
        }
    }

    /// # Panics
    ///
    /// Panics if:
    /// - The seat has no pointer capability (niri always attaches one at startup)
    /// - The output has no known geometry in the global space
    pub fn render_pointer<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
        push: &mut dyn FnMut(PointerRenderElements<R>),
    ) {
        let output_scale = output.current_scale();
        let output_pos = self.global_space.output_geometry(output).unwrap().loc;

        let pointer_pos = self.seat.get_pointer().unwrap().current_location();
        let pointer_pos = pointer_pos - output_pos.to_f64();

        // Get the render cursor to draw.
        let cursor_scale = output_scale.integer_scale();
        let render_cursor = self.cursor_manager.get_render_cursor(cursor_scale);

        let output_scale = Scale::from(output.current_scale().fractional_scale());

        match render_cursor {
            RenderCursor::Hidden => (),
            RenderCursor::Surface { surface, hotspot } => {
                let pointer_pos =
                    (pointer_pos - hotspot.to_f64()).to_physical_precise_round(output_scale);

                push_elements_from_surface_tree(
                    renderer,
                    &surface,
                    pointer_pos,
                    output_scale,
                    1.,
                    Kind::Cursor,
                    &mut |elem| push(elem.into()),
                );
            }
            RenderCursor::Named {
                icon,
                scale,
                cursor,
            } => {
                // XCursor::frame() only uses this to pick an animation frame index modulo the
                // cursor theme's frame durations (typically well under a second each), so
                // wrapping after ~49.7 days of continuous uptime has no visible effect.
                #[allow(clippy::cast_possible_truncation)]
                let elapsed_ms = self.start_time.elapsed().as_millis() as u32;
                let (idx, frame) = cursor.frame(elapsed_ms);
                let hotspot = XCursor::hotspot(frame).to_logical(scale);
                let pointer_pos =
                    (pointer_pos - hotspot.to_f64()).to_physical_precise_round(output_scale);

                let texture = self.cursor_texture_cache.get(icon, scale, &cursor, idx);
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    pointer_pos,
                    &texture,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(element) => push(element.into()),
                    Err(err) => {
                        warn!("error importing a cursor texture: {err:?}");
                    }
                }
            }
        }

        if let Some(dnd_icon) = self.dnd_icon.as_ref() {
            let pointer_pos =
                (pointer_pos + dnd_icon.offset.to_f64()).to_physical_precise_round(output_scale);
            push_elements_from_surface_tree(
                renderer,
                &dnd_icon.surface,
                pointer_pos,
                output_scale,
                1.,
                Kind::ScanoutCandidate,
                &mut |elem| push(elem.into()),
            );
        }
    }

    /// Checks if the pointer should be included on a window cast or screenshot.
    ///
    /// Returns `(cursor_global_pos, win_pos)` if the pointer should be included, or `None`
    /// otherwise.
    ///
    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn pointer_pos_for_window_cast(
        &self,
        mapped: &Mapped,
    ) -> Option<(Point<f64, Logical>, Point<f64, Logical>)> {
        // Regular cursor.
        if let Some((w, HitType::Input { win_pos })) = &self.pointer_contents.window
            && w == &mapped.window
        {
            // Grabs can modify the pointer focus, making it different from
            // pointer_contents. Notably, gestures like Mod+MMB will remove the pointer
            // focus, and ClickGrab will keep pointer focus on the clicked window even
            // while it's moving over a different window.
            //
            // So, double-check that current_focus() (after grabs) also matches the pointer
            // contents.
            let pointer = self.seat.get_pointer().unwrap();

            // The DnD grab is a bit special because it has its own focus (data device)
            // while the pointer focus is cleared. That focus is not currently exposed from
            // Smithay, and showing DnD icons on window screenshots seems useful, so let's
            // just allow it during DnD grabs.
            let is_dnd_grab = pointer
                .with_grab(|_, grab| State::is_dnd_grab(grab.as_any()))
                .unwrap_or(false);

            let current_focus_matches = is_dnd_grab
                || pointer
                    .current_focus()
                    .map(|focused| self.find_root_shell_surface(&focused))
                    .is_some_and(|focused| mapped.is_wl_surface(&focused));
            if current_focus_matches {
                // We don't check for pointer visibility because it can only be Visible or
                // Hidden, and never Disabled (then it wouldn't have focus). Even when the
                // pointer is Hidden, we want to render it, since the user explicitly
                // requested show_pointer = true, and otherwise there's no easy way to
                // screenshot a window with pointer with hide-when-typing because pressing
                // the screenshot bind will hide the pointer.
                return Some((pointer.current_location(), *win_pos));
            }
        }

        None
    }

    /// # Panics
    ///
    /// Panics if:
    /// - The seat has no pointer capability (niri always attaches one at startup)
    /// - The output has no known geometry in the global space
    // The two match arms each loop over every output while accumulating several mutable
    // locals (cursor_scale/cursor_transform, dnd_scale/dnd_transform) that are only sent via
    // with_states()/send_scale_transform() once after the loop; pulling either arm's body out
    // into a helper would mean returning 4+ accumulators just to keep the single post-loop send.
    #[allow(clippy::too_many_lines)]
    pub fn refresh_pointer_outputs(&mut self) {
        if !self.pointer_visibility.is_visible() {
            return;
        }

        let pointer_pos = self.seat.get_pointer().unwrap().current_location();

        match self.cursor_manager.cursor_image() {
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });

                let surface_pos = pointer_pos.to_i32_round() - hotspot;
                let bbox = bbox_from_surface_tree(surface, surface_pos);

                let dnd = self
                    .dnd_icon
                    .as_ref()
                    .map(|icon| &icon.surface)
                    .map(|surface| (surface, bbox_from_surface_tree(surface, surface_pos)));

                // FIXME we basically need to pick the largest scale factor across the overlapping
                // outputs, this is how it's usually done in clients as well.
                let mut cursor_scale = 1.;
                let mut cursor_transform = Transform::Normal;
                let mut dnd_scale = 1.;
                let mut dnd_transform = Transform::Normal;
                for output in self.global_space.outputs() {
                    let geo = self.global_space.output_geometry(output).unwrap();

                    // Compute pointer surface overlap.
                    if let Some(mut overlap) = geo.intersection(bbox) {
                        overlap.loc -= surface_pos;
                        cursor_scale =
                            f64::max(cursor_scale, output.current_scale().fractional_scale());
                        // FIXME: using the largest overlapping or "primary" output transform would
                        // make more sense here.
                        cursor_transform = output.current_transform();
                        output_update(output, Some(overlap), surface);
                    } else {
                        output_update(output, None, surface);
                    }

                    // Compute DnD icon surface overlap.
                    if let Some((surface, bbox)) = dnd {
                        if let Some(mut overlap) = geo.intersection(bbox) {
                            overlap.loc -= surface_pos;
                            dnd_scale =
                                f64::max(dnd_scale, output.current_scale().fractional_scale());
                            // FIXME: using the largest overlapping or "primary" output transform
                            // would make more sense here.
                            dnd_transform = output.current_transform();
                            output_update(output, Some(overlap), surface);
                        } else {
                            output_update(output, None, surface);
                        }
                    }
                }

                with_states(surface, |data| {
                    send_scale_transform(
                        surface,
                        data,
                        output::Scale::Fractional(cursor_scale),
                        cursor_transform,
                    );
                });
                if let Some((surface, _)) = dnd {
                    with_states(surface, |data| {
                        send_scale_transform(
                            surface,
                            data,
                            output::Scale::Fractional(dnd_scale),
                            dnd_transform,
                        );
                    });
                }
            }
            cursor_image => {
                // There's no cursor surface, but there might be a DnD icon.
                let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) else {
                    return;
                };

                let icon = if let CursorImageStatus::Named(icon) = cursor_image {
                    *icon
                } else {
                    CursorIcon::default()
                };

                let mut dnd_scale = 1.;
                let mut dnd_transform = Transform::Normal;
                for output in self.global_space.outputs() {
                    let geo = self.global_space.output_geometry(output).unwrap();

                    // The default cursor is rendered at the right scale for each output, which
                    // means that it may have a different hotspot for each output.
                    let output_scale = output.current_scale().integer_scale();
                    let cursor = self
                        .cursor_manager
                        .get_cursor_with_name(icon, output_scale)
                        .unwrap_or_else(|| self.cursor_manager.get_default_cursor(output_scale));

                    // For simplicity, we always use frame 0 for this computation. Let's hope the
                    // hotspot doesn't change between frames.
                    let hotspot = XCursor::hotspot(&cursor.frames()[0]).to_logical(output_scale);

                    let surface_pos = pointer_pos.to_i32_round() - hotspot;
                    let bbox = bbox_from_surface_tree(surface, surface_pos);

                    if let Some(mut overlap) = geo.intersection(bbox) {
                        overlap.loc -= surface_pos;
                        dnd_scale = f64::max(dnd_scale, output.current_scale().fractional_scale());
                        // FIXME: using the largest overlapping or "primary" output transform would
                        // make more sense here.
                        dnd_transform = output.current_transform();
                        output_update(output, Some(overlap), surface);
                    } else {
                        output_update(output, None, surface);
                    }
                }

                with_states(surface, |data| {
                    send_scale_transform(
                        surface,
                        data,
                        output::Scale::Fractional(dnd_scale),
                        dnd_transform,
                    );
                });
            }
        }
    }

    pub fn refresh_layout(&mut self) {
        // `Layout` is kept as its own arm rather than merged with the other `=> true` cases
        // below: it's the one case where the layout is genuinely focused, not just drawn as
        // active to avoid spurious animations, and that distinction is worth keeping visible.
        #[allow(clippy::match_same_arms)]
        let layout_is_active = match &self.keyboard_focus {
            KeyboardFocus::Layout { .. } => true,
            KeyboardFocus::LayerShell { .. } => false,

            // Draw layout as active in these cases to reduce unnecessary window animations.
            // There's no confusion because these are both fullscreen modes.
            //
            // FIXME: when going into the screenshot UI from a layer-shell focus, and then back to
            // layer-shell, the layout will briefly draw as active, despite never having focus.
            KeyboardFocus::LockScreen { .. }
            | KeyboardFocus::ScreenshotUi
            | KeyboardFocus::ExitConfirmDialog
            | KeyboardFocus::Overview => true,
        };

        self.layout.refresh(layout_is_active);
    }

    pub fn refresh_window_states(&mut self) {
        let config = self.config.borrow();
        self.layout.with_windows_mut(|mapped, _output| {
            mapped.update_tiled_state(config.prefer_no_csd);
        });
        drop(config);
    }

    /// # Panics
    ///
    /// Panics if no X11 support.
    pub fn refresh_window_rules(&mut self) {
        let config = self.config.borrow();
        let window_rules = &config.window_rules;

        let mut windows = vec![];
        let mut outputs = HashSet::new();
        self.layout.with_windows_mut(|mapped, output| {
            if mapped.recompute_window_rules_if_needed(window_rules, self.is_at_startup) {
                windows.push(mapped.window.clone());

                if let Some(output) = output {
                    outputs.insert(output.clone());
                }

                // Since refresh_window_rules() is called after refresh_layout(), we need to update
                // the tiled state right here, so that it's picked up by the following
                // send_pending_configure().
                mapped.update_tiled_state(config.prefer_no_csd);
            }
        });
        drop(config);

        for win in windows {
            self.layout.update_window(&win, None);
            win.toplevel()
                .expect("no X11 support")
                .send_pending_configure();
        }
        for output in outputs {
            self.queue_redraw(&output);
        }
    }

    pub fn advance_animations(&mut self) {
        self.layout.advance_animations();
        self.config_error_notification.advance_animations();
        self.exit_confirm_dialog.advance_animations();
        self.screenshot_ui.advance_animations();

        for state in self.output_state.values_mut() {
            if let Some(transition) = &mut state.screen_transition
                && transition.is_done()
            {
                state.screen_transition = None;
            }
        }
    }

    pub fn update_render_elements(&mut self, output: Option<&Output>) {
        self.layout.update_render_elements(output);

        for (out, state) in &mut self.output_state {
            if output.is_none_or(|output| out == output) {
                let scale = Scale::from(out.current_scale().fractional_scale());
                let transform = out.current_transform();

                if let Some(transition) = &mut state.screen_transition {
                    transition.update_render_elements(scale, transform);
                }

                let layer_map = layer_map_for_output(out);
                for surface in layer_map.layers() {
                    let Some(mapped) = self.mapped_layer_surfaces.get_mut(surface) else {
                        continue;
                    };
                    let Some(geo) = layer_map.layer_geometry(surface) else {
                        continue;
                    };

                    mapped.update_render_elements(geo.size.to_f64());
                }
                drop(layer_map);
            }
        }
    }

    pub fn update_shaders(&mut self) {
        self.layout.update_shaders();

        for mapped in self.mapped_layer_surfaces.values_mut() {
            mapped.update_shaders();
        }
    }

    pub fn render_to_vec<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        output: &Output,
        include_pointer: bool,
    ) -> Vec<OutputRenderElements<R>> {
        let mut elements = Vec::new();
        self.render(ctx, output, include_pointer, &mut |elem| {
            elements.push(elem);
        });
        elements
    }

    pub fn render<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        output: &Output,
        include_pointer: bool,
        push: &mut dyn FnMut(OutputRenderElements<R>),
    ) {
        self.render_inner(ctx, output, include_pointer, push);
    }

    // Each `push(...)` call here appends the next render element from front (pointer) to
    // back (backdrop) for this output, and contents_under() above must hit-test pointer input
    // in the same front-to-back order; splitting this into per-layer helpers would scatter
    // that shared ordering contract across multiple functions instead of one linear sequence.
    #[allow(clippy::too_many_lines)]
    fn render_inner<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        output: &Output,
        include_pointer: bool,
        push: &mut dyn FnMut(OutputRenderElements<R>),
    ) {
        let state = self.output_state.get(output).unwrap();
        let output_scale = Scale::from(output.current_scale().fractional_scale());

        // The pointer goes on the top.
        if include_pointer && self.pointer_visibility.is_visible() {
            self.render_pointer(ctx.renderer, output, &mut |elem| push(elem.into()));
        }

        // Next, the screen transition texture.
        {
            if let Some(transition) = &state.screen_transition {
                push(transition.render(ctx.target).into());
            }
        }

        // Next, the exit confirm dialog.
        self.exit_confirm_dialog
            .render(ctx.renderer, output, &mut |elem| push(elem.into()));

        // Next, the config error notification too.
        if let Some(element) = self.config_error_notification.render(ctx.renderer, output) {
            push(element.into());
        }

        // If the session is locked, draw the lock surface.
        if self.is_locked() {
            if let Some(surface) = state.lock_surface.as_ref() {
                push_elements_from_surface_tree(
                    ctx.renderer,
                    surface.wl_surface(),
                    Point::new(0, 0),
                    output_scale,
                    1.,
                    Kind::ScanoutCandidate,
                    &mut |elem| push(elem.into()),
                );
            }

            // Draw the solid color background.
            push(
                SolidColorRenderElement::from_buffer(
                    &state.lock_color_buffer,
                    (0., 0.),
                    1.,
                    Kind::Unspecified,
                )
                .into(),
            );

            return;
        }

        // Prepare the background elements.
        let backdrop = SolidColorRenderElement::from_buffer(
            &state.backdrop_buffer,
            (0., 0.),
            1.,
            Kind::Unspecified,
        )
        .into();

        // If the screenshot UI is open, draw it.
        if self.screenshot_ui.is_open() {
            self.screenshot_ui
                .render_output(output, ctx.target, &mut |elem| push(elem.into()));

            // Add the backdrop for outputs that were connected while the screenshot UI was open.
            push(backdrop);

            return;
        }

        // Draw the hotkey overlay on top.
        if let Some(element) = self.hotkey_overlay.render(ctx.renderer, output) {
            push(element.into());
        }

        // Don't draw the focus ring on the workspaces while interactively moving above those
        // workspaces, since the interactively-moved window already has a focus ring.
        let focus_ring = !self.layout.interactive_move_is_moving_above_output(output);

        // Get monitor elements.
        let mon = self.layout.monitor_for_output(output).unwrap();
        let zoom = mon.overview_zoom();

        // Get layer-shell elements.
        let layer_map = layer_map_for_output(output);

        // We use macros instead of closures to avoid borrowing issues (renderer and push() go
        // into different functions).
        macro_rules! push_popups_from_layer {
            ($layer:expr, $backdrop:expr, $push:expr) => {{
                self.render_layer_popups(ctx.r(), &layer_map, $layer, $backdrop, $push);
            }};
            ($layer:expr, true) => {{
                push_popups_from_layer!($layer, true, &mut |elem| push(elem.into()));
            }};
            ($layer:expr, $push:expr) => {{
                push_popups_from_layer!($layer, false, $push);
            }};
            ($layer:expr) => {{
                push_popups_from_layer!($layer, false, &mut |elem| push(elem.into()));
            }};
        }
        macro_rules! push_normal_from_layer {
            ($layer:expr, $backdrop:expr, $push:expr) => {{
                self.render_layer_normal(ctx.r(), &layer_map, $layer, $backdrop, $push);
            }};
            ($layer:expr, true) => {{
                push_normal_from_layer!($layer, true, &mut |elem| push(elem.into()));
            }};
            ($layer:expr, $push:expr) => {{
                push_normal_from_layer!($layer, false, $push);
            }};
            ($layer:expr) => {{
                push_normal_from_layer!($layer, false, &mut |elem| push(elem.into()));
            }};
        }

        // The overlay layer elements go next.
        push_popups_from_layer!(Layer::Overlay);
        push_normal_from_layer!(Layer::Overlay);

        // When rendering above the top layer, we put the regular monitor elements first.
        // Otherwise, we will render all layer-shell pop-ups and the top layer on top.
        if mon.render_above_top_layer() {
            self.layout
                .render_interactive_move_for_output(ctx.r(), output, &mut |elem| push(elem.into()));

            mon.render_insert_hint_between_workspaces(ctx.renderer, &mut |elem| push(elem.into()));

            mon.render_workspaces(ctx.r(), focus_ring, &mut |elem| push(elem.into()));

            push_popups_from_layer!(Layer::Top);
            push_normal_from_layer!(Layer::Top);

            push_popups_from_layer!(Layer::Bottom);
            push_popups_from_layer!(Layer::Background);
            push_normal_from_layer!(Layer::Bottom);
            push_normal_from_layer!(Layer::Background);

            // We don't expect more than one workspace when render_above_top_layer().
            if let Some((ws, _geo)) = mon.workspaces_with_render_geo().next() {
                push(ws.render_background().into());
            }
        } else {
            push_popups_from_layer!(Layer::Top);
            push_normal_from_layer!(Layer::Top);

            self.layout
                .render_interactive_move_for_output(ctx.r(), output, &mut |elem| push(elem.into()));

            mon.render_insert_hint_between_workspaces(ctx.renderer, &mut |elem| push(elem.into()));

            // Macro instead of closure to avoid borrowing push().
            macro_rules! process {
                ($geo:expr) => {
                    &mut |elem| {
                        if let Some(elem) = scale_relocate_crop(elem, output_scale, zoom, $geo) {
                            push(elem.into());
                        }
                    }
                };
            }

            for (_ws, geo) in mon.workspaces_with_render_geo() {
                push_popups_from_layer!(Layer::Bottom, process!(geo));
                push_popups_from_layer!(Layer::Background, process!(geo));
            }

            mon.render_workspaces(ctx.r(), focus_ring, &mut |elem| push(elem.into()));

            for (ws, geo) in mon.workspaces_with_render_geo() {
                push_normal_from_layer!(Layer::Bottom, process!(geo));
                push_normal_from_layer!(Layer::Background, process!(geo));

                process!(geo)(ws.render_background());
            }
        }

        mon.render_workspace_shadows(ctx.renderer, &mut |elem| push(elem.into()));

        // Then the backdrop.
        push_popups_from_layer!(Layer::Background, true);
        push_normal_from_layer!(Layer::Background, true);

        push(backdrop);
    }

    fn layers_in_render_order<'a>(
        &'a self,
        layer_map: &'a LayerMap,
        layer: Layer,
        for_backdrop: bool,
    ) -> impl Iterator<Item = (&'a MappedLayer, Rectangle<i32, Logical>)> {
        // LayerMap returns layers in reverse stacking order.
        layer_map.layers_on(layer).rev().filter_map(move |surface| {
            let mapped = self.mapped_layer_surfaces.get(surface)?;

            if for_backdrop != mapped.place_within_backdrop() {
                return None;
            }

            let geo = layer_map.layer_geometry(surface)?;
            Some((mapped, geo))
        })
    }

    fn render_layer_normal<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        layer_map: &LayerMap,
        layer: Layer,
        for_backdrop: bool,
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        for (mapped, geo) in self.layers_in_render_order(layer_map, layer, for_backdrop) {
            let loc = geo.loc.to_f64();
            mapped.render_normal(ctx.r(), loc, push);
        }
    }

    fn render_layer_popups<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        layer_map: &LayerMap,
        layer: Layer,
        for_backdrop: bool,
        push: &mut dyn FnMut(LayerSurfaceRenderElement<R>),
    ) {
        for (mapped, geo) in self.layers_in_render_order(layer_map, layer, for_backdrop) {
            let loc = geo.loc.to_f64();
            mapped.render_popups(ctx.r(), loc, push);
        }
    }

    fn redraw(&mut self, backend: &mut Backend, output: &Output) {
        // Verify our invariant.
        let state = self.output_state.get_mut(output).unwrap();
        assert!(matches!(
            state.redraw_state,
            RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
        ));

        let target_presentation_time = state.frame_clock.next_presentation_time();

        // Freeze the clock at the target time.
        self.clock.set_unadjusted(target_presentation_time);

        self.update_render_elements(Some(output));

        let res = if self.monitors_active {
            let state = self.output_state.get_mut(output).unwrap();
            state.unfinished_animations_remain = self.layout.are_animations_ongoing(Some(output));
            state.unfinished_animations_remain |=
                self.config_error_notification.are_animations_ongoing();
            state.unfinished_animations_remain |= self.exit_confirm_dialog.are_animations_ongoing();
            state.unfinished_animations_remain |= self.screenshot_ui.are_animations_ongoing();
            state.unfinished_animations_remain |= state.screen_transition.is_some();

            // Also keep redrawing if the current cursor is animated.
            state.unfinished_animations_remain |= self
                .cursor_manager
                .is_current_cursor_animated(output.current_scale().integer_scale());

            // Also check layer surfaces.
            if !state.unfinished_animations_remain {
                state.unfinished_animations_remain |= layer_map_for_output(output)
                    .layers()
                    .filter_map(|surface| self.mapped_layer_surfaces.get(surface))
                    .any(MappedLayer::are_animations_ongoing);
            }

            // Render.
            backend.render(self, output, target_presentation_time)
        } else {
            RenderResult::Skipped
        };

        let is_locked = self.is_locked();
        let state = self.output_state.get_mut(output).unwrap();

        if res == RenderResult::Skipped {
            // Update the redraw state on failed render.
            state.redraw_state = if let RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) =
                state.redraw_state
            {
                RedrawState::WaitingForEstimatedVBlank(token)
            } else {
                RedrawState::Idle
            };
        }

        // Update the lock render state on successful render, or if monitors are inactive. When
        // monitors are inactive on a TTY, they have no framebuffer attached, so no sensitive data
        // from a last render will be visible.
        if res != RenderResult::Skipped || !self.monitors_active {
            state.lock_render_state = if is_locked {
                LockRenderState::Locked
            } else {
                LockRenderState::Unlocked
            };
        }

        // If we're in process of locking the session, check if the requirements were met.
        match mem::take(&mut self.lock_state) {
            LockState::Locking(confirmation) => {
                if state.lock_render_state == LockRenderState::Unlocked {
                    // We needed to render a locked frame on this output but failed.
                    self.unlock();
                } else {
                    // Check if all outputs are now locked.
                    let all_locked = self
                        .output_state
                        .values()
                        .all(|state| state.lock_render_state == LockRenderState::Locked);

                    if all_locked {
                        // All outputs are locked, report success.
                        let lock = confirmation.ext_session_lock().clone();
                        confirmation.lock();
                        self.lock_state = LockState::Locked(lock);
                    } else {
                        // Still waiting for other outputs.
                        self.lock_state = LockState::Locking(confirmation);
                    }
                }
            }
            lock_state => self.lock_state = lock_state,
        }

        // Send the frame callbacks.
        //
        // FIXME: The logic here could be a bit smarter. Currently, during an animation, the
        // surfaces that are visible for the very last frame (e.g. because the camera is moving
        // away) will receive frame callbacks, and the surfaces that are invisible but will become
        // visible next frame will not receive frame callbacks (so they will show stale contents for
        // one frame). We could advance the animations for the next frame and send frame callbacks
        // according to the expected new positions.
        //
        // However, this should probably be restricted to sending frame callbacks to more surfaces,
        // to err on the safe side.
        self.send_frame_callbacks(output);
    }

    /// # Panics
    ///
    /// Panics if an internal mutex is poisoned.
    pub fn update_primary_scanout_output(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) {
        // FIXME: potentially tweak the compare function. The default one currently always prefers a
        // higher refresh-rate output, which is not always desirable (i.e. with a very small
        // overlap).
        //
        // While we only have cursors and DnD icons crossing output boundaries though, it doesn't
        // matter all that much.
        let update_surface = |surface: &WlSurface| {
            with_surface_tree_downward(
                surface,
                (),
                |_, _, ()| TraversalAction::DoChildren(()),
                |surface, states, ()| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        states,
                        None,
                        render_element_states,
                        default_primary_scanout_output_compare,
                    );
                },
                |_, _, ()| true,
            );
        };

        if let CursorImageStatus::Surface(surface) = &self.cursor_manager.cursor_image() {
            update_surface(surface);
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            update_surface(surface);
        }

        // We're only updating the current output's windows and layer surfaces. This should be fine
        // as in niri they can only be rendered on a single output at a time.
        //
        // The reason to do this at all is that it keeps track of whether the surface is visible or
        // not in a unified way with the pointer surfaces, which makes the logic elsewhere simpler.

        for mapped in self.layout.windows_for_output(output) {
            let win = &mapped.window;
            let offscreen_data = mapped.offscreen_data();
            let offscreen_data = offscreen_data.as_ref();

            win.with_surfaces(|surface, states| {
                let primary_scanout_output = states
                    .data_map
                    .get_or_insert_threadsafe(Mutex::<PrimaryScanoutOutput>::default);
                let mut primary_scanout_output = primary_scanout_output.lock().unwrap();

                let mut id = Id::from_wayland_resource(surface);

                if let Some(data) = offscreen_data {
                    // We have offscreen data; it's likely that all surfaces are on it.
                    if data.states.element_was_presented(id.clone()) {
                        // If the surface was presented to the offscreen, use the offscreen's id.
                        id = data.id.clone();
                    }

                    // If we the surface wasn't presented to the offscreen it can mean:
                    //
                    // - The surface was invisible. For example, it's obscured by another surface on
                    //   the offscreen, or simply isn't mapped.
                    // - The surface is rendered separately from the offscreen, for example: popups
                    //   during the window resize animation.
                    //
                    // In both of these cases, using the original surface element id and the
                    // original states is the correct thing to do. We may find the surface in the
                    // original states (in the second case). Either way, we definitely know it is
                    // *not* in the offscreen, and we won't miss it.
                    //
                    // There's one edge case: if the surface is both in the offscreen and separate,
                    // and the offscreen itself is invisible, while the separate surface is
                    // visible. In this case we'll currently mark the surface as invisible. We
                    // don't really use offscreens like that however, and if we start, it's easy
                    // enough to fix (need an extra check).
                }

                primary_scanout_output.update_from_render_element_states(
                    id,
                    output,
                    None,
                    render_element_states,
                    |_, _, output, _| output,
                );
            });
        }

        for layer in layer_map_for_output(output).layers() {
            let surface = layer.wl_surface();

            with_surfaces_surface_tree(surface, |surface, states| {
                let primary_scanout_output = states
                    .data_map
                    .get_or_insert_threadsafe(Mutex::<PrimaryScanoutOutput>::default);
                let mut primary_scanout_output = primary_scanout_output.lock().unwrap();
                let id = Id::from_wayland_resource(surface);

                primary_scanout_output.update_from_render_element_states(
                    id,
                    output,
                    None,
                    render_element_states,
                    // Layer surfaces are shown only on one output at a time.
                    |_, _, output, _| output,
                );
            });

            for (popup, _) in PopupManager::popups_for_surface(surface) {
                let surface = popup.wl_surface();
                with_surfaces_surface_tree(surface, |surface, states| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        states,
                        None,
                        render_element_states,
                        // Layer surfaces are shown only on one output at a time.
                        |_, _, output, _| output,
                    );
                });
            }
        }

        if let Some(surface) = &self.output_state[output].lock_surface {
            with_surface_tree_downward(
                surface.wl_surface(),
                (),
                |_, _, ()| TraversalAction::DoChildren(()),
                |surface, states, ()| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        states,
                        None,
                        render_element_states,
                        default_primary_scanout_output_compare,
                    );
                },
                |_, _, ()| true,
            );
        }
    }

    pub fn send_dmabuf_feedbacks(
        &self,
        output: &Output,
        feedback: &SurfaceDmabufFeedback,
        render_element_states: &RenderElementStates,
    ) {
        // We can unconditionally send the current output's feedback to regular and layer-shell
        // surfaces, as they can only be displayed on a single output at a time. Even if a surface
        // is currently invisible, this is the DMABUF feedback that it should know about.
        for mapped in self.layout.windows_for_output(output) {
            mapped.window.send_dmabuf_feedback(
                output,
                |_, _| Some(output.clone()),
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        for surface in layer_map_for_output(output).layers() {
            surface.send_dmabuf_feedback(
                output,
                |_, _| Some(output.clone()),
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        if let Some(surface) = &self.output_state[output].lock_surface {
            send_dmabuf_feedback_surface_tree(
                surface.wl_surface(),
                output,
                |_, _| Some(output.clone()),
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            send_dmabuf_feedback_surface_tree(
                surface,
                output,
                surface_primary_scanout_output,
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        if let CursorImageStatus::Surface(surface) = &self.cursor_manager.cursor_image() {
            send_dmabuf_feedback_surface_tree(
                surface,
                output,
                surface_primary_scanout_output,
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }
    }

    /// # Panics
    ///
    /// Panics if `output` is not present in `output_state`.
    pub fn send_frame_callbacks(&mut self, output: &Output) {
        let state = self.output_state.get(output).unwrap();
        let sequence = state.frame_callback_sequence;

        let should_send = |surface: &WlSurface, states: &SurfaceData| {
            // Do the standard primary scanout output check. For pointer surfaces it deduplicates
            // the frame callbacks across potentially multiple outputs, and for regular windows and
            // layer-shell surfaces it avoids sending frame callbacks to invisible surfaces.
            let current_primary_output = surface_primary_scanout_output(surface, states);
            if current_primary_output.as_ref() != Some(output) {
                return None;
            }

            // Next, check the throttling status.
            let frame_throttling_state = states
                .data_map
                .get_or_insert(SurfaceFrameThrottlingState::default);
            let mut last_sent_at = frame_throttling_state.last_sent_at.borrow_mut();

            // If we already sent a frame callback to this surface this output refresh
            // cycle, don't send one again to prevent empty-damage commit busy loops.
            let send = !matches!(
                &*last_sent_at,
                Some((last_output, last_sequence))
                    if last_output == output && *last_sequence == sequence
            );

            if send {
                *last_sent_at = Some((output.clone(), sequence));
                Some(output.clone())
            } else {
                None
            }
        };

        let frame_callback_time = get_monotonic_time();

        for mapped in self.layout.windows_for_output_mut(output) {
            mapped.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }

        for surface in layer_map_for_output(output).layers() {
            surface.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }

        if let Some(surface) = &self.output_state[output].lock_surface {
            send_frames_surface_tree(
                surface.wl_surface(),
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            send_frames_surface_tree(
                surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            send_frames_surface_tree(
                surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }
    }

    pub fn send_frame_callbacks_on_fallback_timer(&mut self) {
        // Make up a bogus output; we don't care about it here anyway, just the throttling timer.
        let output = Output::new(
            String::new(),
            PhysicalProperties {
                size: Size::from((0, 0)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        );
        let output = &output;

        let frame_callback_time = get_monotonic_time();

        self.layout.with_windows_mut(|mapped, _| {
            mapped.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
        });

        for (output, state) in &self.output_state {
            for surface in layer_map_for_output(output).layers() {
                surface.send_frame(
                    output,
                    frame_callback_time,
                    FRAME_CALLBACK_THROTTLE,
                    |_, _| None,
                );
            }

            if let Some(surface) = &state.lock_surface {
                send_frames_surface_tree(
                    surface.wl_surface(),
                    output,
                    frame_callback_time,
                    FRAME_CALLBACK_THROTTLE,
                    |_, _| None,
                );
            }
        }

        if let Some(surface) = &self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            send_frames_surface_tree(
                surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
        }

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            send_frames_surface_tree(
                surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
        }
    }

    pub fn take_presentation_feedbacks(
        &mut self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);

        if let CursorImageStatus::Surface(surface) = &self.cursor_manager.cursor_image() {
            take_presentation_feedback_surface_tree(
                surface,
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            take_presentation_feedback_surface_tree(
                surface,
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        for mapped in self.layout.windows_for_output(output) {
            mapped.window.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        for surface in layer_map_for_output(output).layers() {
            surface.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        if let Some(surface) = &self.output_state[output].lock_surface {
            take_presentation_feedback_surface_tree(
                surface.wl_surface(),
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        feedback
    }

    /// # Panics
    ///
    /// Panics if `output` has no current mode set.
    pub fn capture_screenshots<'a>(
        &'a self,
        renderer: &'a mut GlesRenderer,
    ) -> impl Iterator<Item = (Output, [OutputScreenshot; 2])> + 'a {
        self.global_space.outputs().cloned().filter_map(|output| {
            let size = output.current_mode().unwrap().size;
            let transform = output.current_transform();
            let size = transform.transform_size(size);

            let scale = Scale::from(output.current_scale().fractional_scale());
            let targets = [RenderTarget::Output, RenderTarget::ScreenCapture];
            let screenshot = targets.map(|target| {
                let ctx = RenderCtx { renderer, target };
                let elements = self.render_to_vec(ctx, &output, false);
                let elements = elements.iter().rev();

                let res = render_to_texture(
                    renderer,
                    size,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements,
                );
                if let Err(err) = &res {
                    warn!("error rendering output {}: {err:?}", output.name());
                }
                let res_output = res.ok();

                let mut pointer = Vec::new();

                // We check the pointer visibility for Disabled (and not .is_visible()) in order to
                // show the pointer even when it's hidden through cursor {} options. The user can
                // then toggle it in the screenshot UI as needed.
                if self.pointer_visibility != PointerVisibility::Disabled {
                    self.render_pointer(renderer, &output, &mut |elem| pointer.push(elem));
                }

                let res_pointer = if pointer.is_empty() {
                    None
                } else {
                    let res = render_to_encompassing_texture(
                        renderer,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        &pointer,
                    );
                    if let Err(err) = &res {
                        warn!("error rendering pointer for {}: {err:?}", output.name());
                    }
                    res.ok()
                };

                res_output.map(|(texture, _)| {
                    OutputScreenshot::from_textures(
                        renderer,
                        scale,
                        texture,
                        res_pointer.map(|(texture, _, geo)| (texture, geo)),
                    )
                })
            });

            if screenshot.iter().any(Option::is_none) {
                return None;
            }

            let screenshot = screenshot.map(|res| res.unwrap());
            Some((output, screenshot))
        })
    }

    /// # Panics
    ///
    /// Panics if `output` has no current mode set.
    pub fn screenshot(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        write_to_disk: bool,
        include_pointer: bool,
        path: Option<String>,
    ) -> anyhow::Result<()> {
        self.update_render_elements(Some(output));

        let size = output.current_mode().unwrap().size;
        let transform = output.current_transform();
        let size = transform.transform_size(size);

        let scale = Scale::from(output.current_scale().fractional_scale());
        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::ScreenCapture,
        };
        let elements = self.render_to_vec(ctx, output, include_pointer);
        let elements = elements.iter().rev();
        let pixels = render_to_vec(
            renderer,
            size,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        )?;

        self.save_screenshot(size, pixels, write_to_disk, path)
            .context("error saving screenshot")
    }

    pub fn screenshot_window(
        &self,
        renderer: &mut GlesRenderer,
        output: &Output,
        mapped: &Mapped,
        write_to_disk: bool,
        show_pointer: bool,
        path: Option<String>,
    ) -> anyhow::Result<()> {
        let scale = Scale::from(output.current_scale().fractional_scale());
        let alpha =
            if mapped.sizing_mode().is_fullscreen() || mapped.is_ignoring_opacity_window_rule() {
                1.
            } else {
                mapped.rules().opacity.unwrap_or(1.).clamp(0., 1.)
            };

        let mut elements: Vec<WindowScreenshotRenderElement<GlesRenderer>> = Vec::new();

        // Add pointer if requested and it's over this window.
        if show_pointer && let Some((_, win_pos)) = self.pointer_pos_for_window_cast(mapped) {
            // Pointer elements are at output-local physical coords.
            // Relocate by -win_pos to make them window-relative.
            let pos = win_pos.to_physical_precise_round(scale).upscale(-1);
            self.render_pointer(renderer, output, &mut |elem| {
                let elem = RelocateRenderElement::from_element(elem, pos, Relocate::Relative);
                elements.push(elem.into());
            });
        }
        let pointer_count = elements.len();

        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::ScreenCapture,
        };
        mapped.render(
            ctx,
            mapped.window.geometry().loc.to_f64(),
            scale,
            alpha,
            &mut |elem| elements.push(elem.into()),
        );

        // The pointer is not included in encompassing_geo because we don't want it to expand the
        // screenshot size.
        let geo = encompassing_geo(scale, elements.iter().skip(pointer_count));
        let elements = elements.iter().rev().map(|elem| {
            RelocateRenderElement::from_element(elem, geo.loc.upscale(-1), Relocate::Relative)
        });
        let pixels = render_to_vec(
            renderer,
            geo.size,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        )?;

        self.save_screenshot(geo.size, pixels, write_to_disk, path)
            .context("error saving screenshot")
    }

    /// # Panics
    ///
    /// Panics if an internal invariant is violated.
    pub fn save_screenshot(
        &self,
        size: Size<i32, Physical>,
        pixels: Vec<u8>,
        write_to_disk: bool,
        path_arg: Option<String>,
    ) -> anyhow::Result<()> {
        let path = write_to_disk
            .then(|| {
                // When given an explicit path, don't try to strftime it or create parents.
                path_arg.map(|p| (PathBuf::from(p), false)).or_else(|| {
                    match make_screenshot_path(&self.config.borrow()) {
                        Ok(path) => path.map(|p| (p, true)),
                        Err(err) => {
                            warn!("error making screenshot path: {err:?}");
                            None
                        }
                    }
                })
            })
            .flatten();

        // Prepare to set the encoded image as our clipboard selection. This must be done from the
        // main thread.
        let (tx, rx) = calloop::channel::sync_channel::<Arc<[u8]>>(1);
        self.event_loop
            .insert_source(rx, move |event, (), state| match event {
                calloop::channel::Event::Msg(buf) => {
                    set_data_device_selection(
                        &state.niri.display_handle,
                        &state.niri.seat,
                        vec![String::from("image/png")],
                        buf,
                    );
                }
                calloop::channel::Event::Closed => (),
            })
            .unwrap();

        // Prepare to send screenshot completion event back to main thread.
        let (event_tx, event_rx) = calloop::channel::sync_channel::<Option<String>>(1);
        self.event_loop
            .insert_source(event_rx, move |event, (), state| match event {
                calloop::channel::Event::Msg(path) => {
                    state.ipc_screenshot_taken(path);
                }
                calloop::channel::Event::Closed => (),
            })
            .unwrap();

        // Encode and save the image in a thread as it's slow.
        thread::spawn(move || {
            let mut buf = vec![];

            let w = std::io::Cursor::new(&mut buf);
            // `size` is the dimensions of an already-captured framebuffer region, which is
            // always non-negative (a negative width/height would mean the capture itself was
            // already broken, long before we get here), so this cannot actually lose a sign.
            #[allow(clippy::cast_sign_loss)]
            let (w_px, h_px) = (size.w as u32, size.h as u32);
            if let Err(err) = write_png_rgba8(w, w_px, h_px, &pixels) {
                warn!("error encoding screenshot image: {err:?}");
                return;
            }

            let buf: Arc<[u8]> = Arc::from(buf.into_boxed_slice());
            let _ = tx.send(buf.clone());

            let mut image_path = None;

            if let Some((path, create_parent)) = path {
                debug!("saving screenshot to {path:?}");

                if create_parent && let Some(parent) = path.parent() {
                    // Relative paths with one component, i.e. "test.png", have Some("") parent.
                    if !parent.as_os_str().is_empty()
                        && let Err(err) = std::fs::create_dir_all(parent)
                        && err.kind() != std::io::ErrorKind::AlreadyExists
                    {
                        warn!("error creating screenshot directory: {err:?}");
                    }
                }

                match std::fs::write(&path, buf) {
                    Ok(()) => image_path = Some(path),
                    Err(err) => {
                        warn!("error saving screenshot image: {err:?}");
                    }
                }
            } else {
                debug!("not saving screenshot to disk");
            }

            // Send screenshot completion event.
            let path_string = image_path
                .as_ref()
                .and_then(|p| p.to_str())
                .map(ToOwned::to_owned);
            let _ = event_tx.send(path_string);
        });

        Ok(())
    }

    pub const fn is_locked(&self) -> bool {
        match self.lock_state {
            LockState::Unlocked | LockState::WaitingForSurfaces { .. } => false,
            LockState::Locking(_) | LockState::Locked(_) => true,
        }
    }

    /// # Panics
    ///
    /// Panics if an internal invariant is violated.
    pub fn lock(&mut self, confirmation: SessionLocker) {
        // Check if another client is in the process of locking.
        if matches!(
            self.lock_state,
            LockState::WaitingForSurfaces { .. } | LockState::Locking(_)
        ) {
            info!("refusing lock as another client is currently locking");
            return;
        }

        // Check if we're already locked with an active client.
        if let LockState::Locked(lock) = &self.lock_state {
            if lock.is_alive() {
                info!("refusing lock as already locked with an active client");
                return;
            }

            // If the client had died, continue with the new lock.
            info!("locking session (replacing existing dead lock)");

            // Since the session was already locked, we know that the outputs are blanked, and
            // can lock right away.
            let lock = confirmation.ext_session_lock().clone();
            confirmation.lock();
            self.lock_state = LockState::Locked(lock);

            return;
        }

        info!("locking session");

        if self.output_state.is_empty() {
            // There are no outputs, lock the session right away.
            self.screenshot_ui.close();
            self.cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());

            let lock = confirmation.ext_session_lock().clone();
            confirmation.lock();
            self.lock_state = LockState::Locked(lock);
        } else {
            // There are outputs which we need to redraw before locking. But before we do that,
            // let's wait for the lock surfaces.
            //
            // Give them a second; swaylock can take its time to paint a big enough image.
            let timer = Timer::from_duration(Duration::from_secs(1));
            let deadline_token = self
                .event_loop
                .insert_source(timer, |_, (), state| {
                    trace!("lock deadline expired, continuing");
                    state.niri.continue_to_locking();
                    TimeoutAction::Drop
                })
                .unwrap();

            self.lock_state = LockState::WaitingForSurfaces {
                confirmation,
                deadline_token,
            };
        }
    }

    pub fn maybe_continue_to_locking(&mut self) {
        if !matches!(self.lock_state, LockState::WaitingForSurfaces { .. }) {
            // Not waiting.
            return;
        }

        // Check if there are any outputs whose lock surfaces had not had a commit yet.
        for state in self.output_state.values() {
            let Some(surface) = &state.lock_surface else {
                // Surface not created yet.
                return;
            };

            if !is_mapped(surface.wl_surface()) {
                return;
            }
        }

        // All good.
        trace!("lock surfaces are ready, continuing");
        self.continue_to_locking();
    }

    fn continue_to_locking(&mut self) {
        match mem::take(&mut self.lock_state) {
            LockState::WaitingForSurfaces {
                confirmation,
                deadline_token,
            } => {
                self.event_loop.remove(deadline_token);

                self.screenshot_ui.close();
                self.cursor_manager
                    .set_cursor_image(CursorImageStatus::default_named());

                if self.output_state.is_empty() {
                    // There are no outputs, lock the session right away.
                    let lock = confirmation.ext_session_lock().clone();
                    confirmation.lock();
                    self.lock_state = LockState::Locked(lock);
                } else {
                    // There are outputs which we need to redraw before locking.
                    self.lock_state = LockState::Locking(confirmation);
                    self.queue_redraw_all();
                }
            }
            other => {
                error!("continue_to_locking() called with wrong lock state: {other:?}",);
                self.lock_state = other;
            }
        }
    }

    pub fn unlock(&mut self) {
        info!("unlocking session");

        let prev = mem::take(&mut self.lock_state);
        if let LockState::WaitingForSurfaces { deadline_token, .. } = prev {
            self.event_loop.remove(deadline_token);
        }

        for output_state in self.output_state.values_mut() {
            output_state.lock_surface = None;
        }
        self.queue_redraw_all();
    }

    pub fn new_lock_surface(&mut self, surface: LockSurface, output: &Output) {
        let lock = match &self.lock_state {
            LockState::Unlocked => {
                error!("tried to add a lock surface on an unlocked session");
                return;
            }
            LockState::WaitingForSurfaces { confirmation, .. }
            | LockState::Locking(confirmation) => confirmation.ext_session_lock(),
            LockState::Locked(lock) => lock,
        };

        if lock.client() != surface.wl_surface().client() {
            debug!("ignoring lock surface from an unrelated client");
            return;
        }

        let Some(output_state) = self.output_state.get_mut(output) else {
            error!("missing output state");
            return;
        };

        output_state.lock_surface = Some(surface);
    }

    /// Activates the pointer constraint if necessary according to the current pointer contents.
    ///
    /// Make sure the pointer location and contents are up to date before calling this.
    ///
    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn maybe_activate_pointer_constraint(&self) {
        let Some((surface, surface_loc)) = &self.pointer_contents.surface else {
            return;
        };

        let pointer = self.seat.get_pointer().unwrap();
        if Some(surface) != pointer.current_focus().as_ref() {
            return;
        }

        with_pointer_constraint(surface, &pointer, |constraint| {
            let Some(constraint) = constraint else { return };

            if constraint.is_active() {
                return;
            }

            // Constraint does not apply if not within region.
            if let Some(region) = constraint.region() {
                let pointer_pos = pointer.current_location();
                let pos_within_surface = pointer_pos - *surface_loc;
                if !region.contains(pos_within_surface.to_i32_round()) {
                    return;
                }
            }

            constraint.activate();
        });
    }

    pub fn focus_layer_surface_if_on_demand(&mut self, surface: Option<LayerSurface>) {
        if let Some(surface) = surface
            && surface.cached_state().keyboard_interactivity
                == wlr_layer::KeyboardInteractivity::OnDemand
        {
            if self.layer_shell_on_demand_focus.as_ref() != Some(&surface) {
                self.layer_shell_on_demand_focus = Some(surface);

                // FIXME: granular.
                self.queue_redraw_all();
            }

            return;
        }

        // Something else got clicked, clear on-demand layer-shell focus.
        if self.layer_shell_on_demand_focus.is_some() {
            self.layer_shell_on_demand_focus = None;

            // FIXME: granular.
            self.queue_redraw_all();
        }
    }

    /// Tries to find and return the root shell surface for a given surface.
    ///
    /// I.e. for popups, this function will try to find the parent toplevel or layer surface. For
    /// regular subsurfaces, it will find the root surface.
    pub fn find_root_shell_surface(&self, surface: &WlSurface) -> WlSurface {
        let Some(root) = self.root_surface.get(surface) else {
            return surface.clone();
        };

        if let Some(popup) = self.popups.find_popup(root) {
            return find_popup_root_surface(&popup).unwrap_or_else(|_| root.clone());
        }

        root.clone()
    }

    /// # Panics
    ///
    /// Panics if the seat has no pointer capability (niri always attaches one at startup).
    pub fn handle_focus_follows_mouse(&mut self, new_focus: &PointContents) {
        let Some(ffm) = self.config.borrow().input.focus_follows_mouse else {
            return;
        };

        let pointer = &self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() {
            return;
        }

        // Recompute the current pointer focus because we don't update it during animations.
        let current_focus = self.contents_under(pointer.current_location());

        if let Some(output) = &new_focus.output
            && current_focus.output.as_ref() != Some(output)
        {
            self.layout.focus_output(output);
        }

        if let Some(window) = &new_focus.window
            && !self.layout.is_overview_open()
            && current_focus.window.as_ref() != Some(window)
        {
            let (window, hit) = window;

            // Don't trigger focus-follows-mouse over the tab indicator.
            if matches!(
                hit,
                HitType::Activate {
                    is_tab_indicator: true
                }
            ) {
                return;
            }

            if !self.layout.should_trigger_focus_follows_mouse_on(window) {
                return;
            }

            if let Some(threshold) = ffm.max_scroll_amount
                && self.layout.scroll_amount_to_activate(window) > threshold.0
            {
                return;
            }

            self.layout.activate_window_without_raising(window);
            self.layer_shell_on_demand_focus = None;
        }

        if let Some(layer) = &new_focus.layer
            && current_focus.layer.as_ref() != Some(layer)
        {
            self.layer_shell_on_demand_focus = Some(layer.clone());
        }
    }

    /// # Panics
    ///
    /// Panics if:
    /// - `output` has no current mode set
    /// - `output` is not present in `output_state`
    pub fn do_screen_transition(&mut self, renderer: &mut GlesRenderer, delay_ms: Option<u16>) {
        self.update_render_elements(None);

        let textures: Vec<_> = self
            .output_state
            .keys()
            .cloned()
            .filter_map(|output| {
                let size = output.current_mode().unwrap().size;
                let transform = output.current_transform();

                let scale = Scale::from(output.current_scale().fractional_scale());
                let targets = [RenderTarget::Output, RenderTarget::ScreenCapture];
                let textures = targets.map(|target| {
                    let ctx = RenderCtx { renderer, target };
                    let elements = self.render_to_vec(ctx, &output, false);
                    let elements = elements.iter().rev();

                    let res = render_to_texture(
                        renderer,
                        size,
                        scale,
                        transform,
                        Fourcc::Abgr8888,
                        elements,
                    );

                    if let Err(err) = &res {
                        warn!("error rendering output {}: {err:?}", output.name());
                    }

                    res
                });

                if textures.iter().any(Result::is_err) {
                    return None;
                }

                let textures = textures.map(|res| {
                    let texture = res.unwrap().0;
                    TextureBuffer::from_texture(
                        renderer,
                        texture,
                        scale,
                        transform,
                        Vec::new(), // We want windows below to get frame callbacks.
                    )
                });

                Some((output, textures))
            })
            .collect();

        let delay = delay_ms.map_or(screen_transition::DELAY, |d| {
            Duration::from_millis(u64::from(d))
        });

        for (output, from_texture) in textures {
            let state = self.output_state.get_mut(&output).unwrap();
            state.screen_transition = Some(ScreenTransition::new(
                from_texture,
                delay,
                self.clock.clone(),
            ));
        }

        // We don't actually need to queue a redraw because the point is to freeze the screen for a
        // bit, and even if the delay was zero, we're drawing the same contents anyway.
    }

    pub fn recompute_window_rules(&mut self) {
        let changed = {
            let window_rules = &self.config.borrow().window_rules;

            for unmapped in self.unmapped_windows.values_mut() {
                let new_rules = ResolvedWindowRules::compute(
                    window_rules,
                    WindowRef::Unmapped(unmapped),
                    self.is_at_startup,
                );
                if let InitialConfigureState::Configured { rules, .. } = &mut unmapped.state {
                    *rules = new_rules;
                }
            }

            let mut windows = vec![];
            self.layout.with_windows_mut(|mapped, _| {
                if mapped.recompute_window_rules(window_rules, self.is_at_startup) {
                    windows.push(mapped.window.clone());
                }
            });
            let changed = !windows.is_empty();
            for win in windows {
                self.layout.update_window(&win, None);
            }
            changed
        };

        if changed {
            // FIXME: granular.
            self.queue_redraw_all();
        }
    }

    pub fn recompute_layer_rules(&mut self) {
        let mut changed = false;
        {
            let config = self.config.borrow();
            let rules = &config.layer_rules;

            for mapped in self.mapped_layer_surfaces.values_mut() {
                if mapped.recompute_layer_rules(rules, self.is_at_startup) {
                    changed = true;
                    mapped.update_config(&config);
                }
            }
        }

        if changed {
            // FIXME: granular.
            self.queue_redraw_all();
        }
    }

    /// # Panics
    ///
    /// Panics if an internal invariant is violated.
    pub fn reset_pointer_inactivity_timer(&mut self) {
        if self.pointer_inactivity_timer_got_reset {
            return;
        }

        if let Some(token) = self.pointer_inactivity_timer.take() {
            self.event_loop.remove(token);
        }

        let Some(timeout_ms) = self.config.borrow().cursor.hide_after_inactive_ms else {
            return;
        };

        let duration = Duration::from_millis(u64::from(timeout_ms));
        let timer = Timer::from_duration(duration);
        let token = self
            .event_loop
            .insert_source(timer, move |_, (), state| {
                state.niri.pointer_inactivity_timer = None;

                // If the pointer is already invisible, don't reset it back to Hidden causing one
                // frame of hover.
                if state.niri.pointer_visibility.is_visible() {
                    state.niri.pointer_visibility = PointerVisibility::Hidden;
                    state.niri.queue_redraw_all();
                }

                TimeoutAction::Drop
            })
            .unwrap();
        self.pointer_inactivity_timer = Some(token);

        self.pointer_inactivity_timer_got_reset = true;
    }

    pub const fn notify_activity(&mut self) {
        if self.notified_activity_this_iteration {
            return;
        }

        self.notified_activity_this_iteration = true;
    }
}

pub struct NewClient {
    pub client: UnixStream,
    pub restricted: bool,
    pub credentials_unknown: bool,
}

// can_view_decoration_globals, primary_selection_disabled, restricted, and
// credentials_unknown are four independent per-client Wayland protocol permission
// checks read individually at unrelated call sites (global filters, selection
// handlers, security-context checks); combining them into one flags value would
// not simplify any of those call sites and would just add a bitflags dependency
// to this one struct.
#[allow(clippy::struct_excessive_bools)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub can_view_decoration_globals: bool,
    pub primary_selection_disabled: bool,
    /// Whether this client is denied from the restricted protocols such as security-context.
    pub restricted: bool,
    /// We cannot retrieve this client's socket credentials.
    pub credentials_unknown: bool,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

fn scale_relocate_crop<E: Element>(
    elem: E,
    output_scale: Scale<f64>,
    zoom: f64,
    ws_geo: Rectangle<f64, Logical>,
) -> Option<CropRenderElement<RelocateRenderElement<RescaleRenderElement<E>>>> {
    let ws_geo = ws_geo.to_physical_precise_round(output_scale);
    let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
    let elem = RelocateRenderElement::from_element(elem, ws_geo.loc, Relocate::Relative);
    CropRenderElement::from_element(elem, output_scale, ws_geo)
}

niri_render_elements! {
    PointerRenderElements<R> => {
        Wayland = WaylandSurfaceRenderElement<R>,
        NamedPointer = MemoryRenderBufferRenderElement<R>,
    }
}

niri_render_elements! {
    WindowScreenshotRenderElement<R> => {
        Layout = LayoutElementRenderElement<R>,
        Pointer = RelocateRenderElement<PointerRenderElements<R>>,
    }
}

niri_render_elements! {
    OutputRenderElements<R> => {
        Monitor = MonitorRenderElement<R>,
        RescaledTile = RescaleRenderElement<TileRenderElement<R>>,
        LayerSurface = LayerSurfaceRenderElement<R>,
        RelocatedLayerSurface = CropRenderElement<RelocateRenderElement<RescaleRenderElement<
            LayerSurfaceRenderElement<R>
        >>>,
        RelocatedColor = CropRenderElement<RelocateRenderElement<RescaleRenderElement<
            SolidColorRenderElement
        >>>,
        Pointer = PointerRenderElements<R>,
        Wayland = WaylandSurfaceRenderElement<R>,
        SolidColor = SolidColorRenderElement,
        ScreenshotUi = ScreenshotUiRenderElement,
        ExitConfirmDialog = ExitConfirmDialogRenderElement,
        Texture = PrimaryGpuTextureRenderElement,
        // Used for the CPU-rendered panels.
        RelocatedMemoryBuffer = RelocateRenderElement<MemoryRenderBufferRenderElement<R>>,
    }
}
