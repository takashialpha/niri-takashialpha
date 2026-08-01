mod compositor;
mod layer_shell;
mod xdg_shell;

use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::thread;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::input::TabletToolDescriptor;
use smithay::delegate_dispatch2;
use smithay::input::dnd::{self, DnDGrab, DndGrabHandler, DndTarget};
use smithay::input::pointer::{CursorIcon, CursorImageStatus, Focus, PointerHandle};
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::{Seat, SeatHandler, SeatState, keyboard};
use smithay::output::Output;
use smithay::reexports::rustix::fs::{OFlags, fcntl_setfl};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::compositor::{get_parent, with_states};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::FractionalScaleHandler;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::{PointerConstraintsHandler, with_pointer_constraint};
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::wayland::selection::{SelectionHandler, SelectionTarget};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

pub use crate::handlers::xdg_shell::KdeDecorationsModeState;
use crate::niri::{DndIcon, State};
use crate::utils::{output_size, send_scale_transform};

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.niri.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, mut image: CursorImageStatus) {
        // FIXME: this hack should be removable once the screenshot UI is tracked with a
        // PointerFocus properly.
        if self.niri.screenshot_ui.is_open() {
            image = CursorImageStatus::Named(CursorIcon::Crosshair);
        }
        self.niri.cursor_manager.set_cursor_image(image);
        // FIXME: more granular
        self.niri.queue_redraw_all();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.niri.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: keyboard::LedState) {
        let keyboards = self
            .niri
            .devices
            .iter()
            .filter(|device| device.has_capability(input::DeviceCapability::Keyboard))
            .cloned();

        for mut keyboard in keyboards {
            keyboard.led_update(led_state.into());
        }
    }
}

// Required by the cursor-shape protocol (a cursor-shape device can be created from a tablet tool),
// even though niri no longer exposes the tablet manager global itself.
impl TabletSeatHandler for State {
    type ToolFocus = WlSurface;

    fn tablet_tool_image(&mut self, _tool: &TabletToolDescriptor, image: CursorImageStatus) {
        // FIXME: tablet tools should have their own cursors.
        self.niri.cursor_manager.set_cursor_image(image);
        // FIXME: granular.
        self.niri.queue_redraw_all();
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        // Pointer constraints track pointer focus internally, so make sure it's up to date before
        // activating a new one.
        self.refresh_pointer_contents();

        self.niri.maybe_activate_pointer_constraint();
    }

    fn remove_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        // Constraints are re-evaluated on pointer motion and focus changes, so nothing to do here.
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let is_constraint_active = with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|c| c.is_active())
        });

        if !is_constraint_active {
            return;
        }

        // Note: this is surface under pointer, not pointer focus. So if you start, say, a
        // middle-drag in Blender, then touchpad-swipe the window away, the surface under pointer
        // will change, even though the real pointer focus remains on the Blender surface due to
        // the click grab.
        //
        // Ideally we would just use the constraint surface, but we need its origin. So this is
        // more of a hack because pointer contents has the surface origin available.
        //
        // FIXME: use the constraint surface somehow, don't use pointer contents.
        let Some((ref surface_under_pointer, origin)) = self.niri.pointer_contents.surface else {
            return;
        };

        if surface_under_pointer != surface {
            return;
        }

        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        let target = self
            .niri
            .output_for_root(&root)
            .and_then(|output| self.niri.global_space.output_geometry(output))
            .map_or(origin + location, |mut output_geometry| {
                // i32 sizes are exclusive, but f64 sizes are inclusive.
                output_geometry.size -= (1, 1).into();
                (origin + location).constrain(output_geometry.to_f64())
            });
        pointer.set_location(target);

        // Redraw to update the cursor position if it's visible.
        if self.niri.pointer_visibility.is_visible() {
            // FIXME: redraw only outputs overlapping the cursor.
            self.niri.queue_redraw_all();
        }
    }
}

impl SelectionHandler for State {
    type SelectionUserData = Arc<[u8]>;

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        let buf = user_data.clone();
        thread::spawn(move || {
            // Clear O_NONBLOCK, otherwise File::write_all() will stop halfway.
            if let Err(err) = fcntl_setfl(&fd, OFlags::empty()) {
                warn!("error clearing flags on selection target fd: {err:?}");
            }
            if let Err(err) = File::from(fd).write_all(&buf) {
                warn!("error writing selection: {err:?}");
            }
        });
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.niri.data_device_state
    }
}

impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: dnd::Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: dnd::GrabType,
    ) {
        self.niri.dnd_icon = icon.map(|surface| DndIcon {
            surface,
            offset: Point::new(0, 0),
        });

        match type_ {
            dnd::GrabType::Pointer => {
                let pointer = seat.get_pointer().unwrap();
                let start_data = pointer.grab_start_data().unwrap();
                let grab =
                    DnDGrab::new_pointer(&self.niri.display_handle, start_data, source, seat);
                pointer.set_grab(self, grab, serial, Focus::Keep);
            }
            // Touch input is not supported, so a touch-initiated DnD can never start.
            dnd::GrabType::Touch => unreachable!("touch input is not supported"),
        }

        // FIXME: more granular
        self.niri.queue_redraw_all();
    }
}

impl DndGrabHandler for State {
    fn dropped(
        &mut self,
        target: Option<DndTarget<'_, Self>>,
        validated: bool,
        _seat: Seat<Self>,
        location: Point<f64, Logical>,
    ) {
        let target: Option<&WlSurface> = target.map(DndTarget::into_inner);
        trace!("dnd dropped, target: {target:?}, validated: {validated}");

        // End DnD before activating a specific window below so that it takes precedence.
        self.niri.on_maybe_dnd_ended();

        // Activate the target output, since that's how Firefox drag-tab-into-new-window works for
        // example. On successful drop, additionally activate the target window.
        let mut activate_output = true;
        if let Some(target) = validated.then_some(target).flatten() {
            let root = self.niri.find_root_shell_surface(target);
            if let Some((mapped, _)) = self.niri.layout.find_window_and_output(&root) {
                let window = mapped.window.clone();
                self.niri.layout.activate_window(&window);
                self.niri.layer_shell_on_demand_focus = None;
                activate_output = false;
            }
        }

        if activate_output {
            // Find the output from drop coordinates.
            if let Some((output, _)) = self.niri.output_under(location) {
                let output = output.clone();
                self.niri.layout.focus_output(&output);
            }
        }
    }

    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        trace!("dnd cancelled");

        self.niri.on_maybe_dnd_ended();
    }
}

impl crate::niri::Niri {
    fn on_maybe_dnd_ended(&mut self) {
        self.layout.dnd_end();
        self.dnd_icon = None;
        // FIXME: more granular
        self.queue_redraw_all();
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.niri.primary_selection_state
    }
}

impl OutputHandler for State {}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.niri.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.backend.import_dmabuf(&dmabuf) {
            let _ = notifier.successful::<Self>();
        } else {
            notifier.failed();
        }
    }
}

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.niri.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        self.niri.lock(confirmation);
    }

    fn unlock(&mut self) {
        self.niri.unlock();
        self.niri.activate_monitors(&mut self.backend);
        self.niri.notify_activity();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = self.niri.output_from_resource(&output) else {
            warn!("no Output matching WlOutput");
            return;
        };

        configure_lock_surface(&surface, &output);
        self.niri.new_lock_surface(surface, &output);
    }
}

pub fn configure_lock_surface(surface: &LockSurface, output: &Output) {
    surface.with_pending_state(|states| {
        let size = output_size(output);
        states.size = Some(size.to_i32_round());
    });
    let scale = output.current_scale();
    let transform = output.current_transform();
    let wl_surface = surface.wl_surface();
    with_states(wl_surface, |data| {
        send_scale_transform(wl_surface, data, scale, transform);
    });
    surface.send_configure();
}

impl FractionalScaleHandler for State {}

// Single blanket delegation for all Wayland protocols; replaces the per-protocol delegate_*!
// macros (removed upstream in favor of Dispatch2/GlobalDispatch2).
delegate_dispatch2!(State);
