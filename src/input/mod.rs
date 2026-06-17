use std::any::Any;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use niri_config::{Action, Bind, Binds, Config, Key, ModKey, Modifiers, Trigger};
use niri_ipc::LayoutSwitchTarget;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputEvent, KeyState,
    KeyboardKeyEvent, Keycode, MouseButton, PointerAxisEvent, PointerButtonEvent,
    PointerMotionEvent,
};
use smithay::backend::libinput::LibinputInputBackend;
use smithay::input::dnd::DnDGrab;
use smithay::input::keyboard::{FilterResult, Keysym, Layout, ModifiersState, keysyms};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus,
    GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, RelativeMotionEvent,
};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
use smithay::wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint};

use self::move_grab::MoveGrab;
use self::pick_color_grab::PickColorGrab;
use self::pick_window_grab::PickWindowGrab;
use self::resize_grab::ResizeGrab;
use self::spatial_movement_grab::SpatialMovementGrab;
use crate::layout::scrolling::ScrollDirection;
use crate::layout::{ActivateWindow, LayoutElement as _};
use crate::niri::{PointerVisibility, State};
use crate::ui::screenshot_ui::ScreenshotUi;
use crate::utils::spawning::{spawn, spawn_sh};
use crate::utils::{ResizeEdge, center, get_monotonic_time};

pub mod backend_ext;
pub mod move_grab;
pub mod pick_color_grab;
pub mod pick_window_grab;
pub mod resize_grab;
pub mod scroll_tracker;
pub mod spatial_movement_grab;
pub mod swipe_tracker;

use backend_ext::{NiriInputBackend as InputBackend, NiriInputDevice as _};

pub const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(400);

impl State {
    pub fn process_input_event<I: InputBackend + 'static>(&mut self, event: InputEvent<I>)
    where
        I::Device: 'static, // Needed for downcasting.
    {
        // Make sure some logic like workspace clean-up has a chance to run before doing actions.
        self.niri.advance_animations();

        if self.niri.monitors_active {
            // Notify the idle-notifier of activity.
            if should_notify_activity(&event) {
                self.niri.notify_activity();
            }
        } else {
            // Power on monitors if they were off.
            if should_activate_monitors(&event) {
                self.niri.activate_monitors(&mut self.backend);

                // Notify the idle-notifier of activity only if we're also powering on the
                // monitors.
                self.niri.notify_activity();
            }
        }

        if should_reset_pointer_inactivity_timer(&event) {
            self.niri.reset_pointer_inactivity_timer();
        }

        let hide_hotkey_overlay =
            self.niri.hotkey_overlay.is_open() && should_hide_hotkey_overlay(&event);

        let hide_exit_confirm_dialog =
            self.niri.exit_confirm_dialog.is_open() && should_hide_exit_confirm_dialog(&event);

        let mut consumed_by_a11y = false;
        use InputEvent::*;
        match event {
            DeviceAdded { .. } | DeviceRemoved { .. } => (),
            Keyboard { event } => self.on_keyboard::<I>(event, &mut consumed_by_a11y),
            PointerMotion { event } => self.on_pointer_motion::<I>(event),
            PointerMotionAbsolute { event } => self.on_pointer_motion_absolute::<I>(event),
            PointerButton { event } => self.on_pointer_button::<I>(event),
            PointerAxis { event } => self.on_pointer_axis::<I>(event),
            // Touchpads, touchscreens, drawing tablets, and hardware switches are not supported;
            // ignore their events. Mouse-wheel scrolling is handled via PointerAxis above.
            GestureSwipeBegin { .. }
            | GestureSwipeUpdate { .. }
            | GestureSwipeEnd { .. }
            | GesturePinchBegin { .. }
            | GesturePinchUpdate { .. }
            | GesturePinchEnd { .. }
            | GestureHoldBegin { .. }
            | GestureHoldEnd { .. }
            | TouchDown { .. }
            | TouchMotion { .. }
            | TouchUp { .. }
            | TouchCancel { .. }
            | TouchFrame { .. }
            | SwitchToggle { .. }
            | TabletToolAxis { .. }
            | TabletToolProximity { .. }
            | TabletToolTip { .. }
            | TabletToolButton { .. } => (),
            Special(_) => (),
        }

        // Don't hide overlays if consumed by a11y, so that you can use the screen reader
        // navigation keys.
        if consumed_by_a11y {
            return;
        }

        // Do this last so that screenshot still gets it.
        if hide_hotkey_overlay && self.niri.hotkey_overlay.hide() {
            self.niri.queue_redraw_all();
        }

        if hide_exit_confirm_dialog && self.niri.exit_confirm_dialog.hide() {
            self.niri.queue_redraw_all();
        }
    }

    pub fn process_libinput_event(&mut self, event: &mut InputEvent<LibinputInputBackend>) {
        match event {
            InputEvent::DeviceAdded { device } => {
                self.niri.devices.insert(device.clone());

                if device.has_capability(input::DeviceCapability::Keyboard)
                    && let Some(led_state) = self
                        .niri
                        .seat
                        .get_keyboard()
                        .map(|keyboard| keyboard.led_state())
                {
                    device.led_update(led_state.into());
                }

                apply_libinput_settings(&self.niri.config.borrow().input, device);
            }
            InputEvent::DeviceRemoved { device } => {
                self.niri.devices.remove(device);
            }
            _ => (),
        }
    }

    /// Computes the rectangle that covers all outputs in global space.
    fn global_bounding_rectangle(&self) -> Option<Rectangle<i32, Logical>> {
        self.niri.global_space.outputs().fold(
            None,
            |acc: Option<Rectangle<i32, Logical>>, output| {
                self.niri
                    .global_space
                    .output_geometry(output)
                    .map(|geo| acc.map(|acc| acc.merge(geo)).unwrap_or(geo))
            },
        )
    }

    fn on_keyboard<I: InputBackend>(
        &mut self,
        event: I::KeyboardKeyEvent,
        consumed_by_a11y: &mut bool,
    ) {
        let mod_key = self.backend.mod_key(&self.niri.config.borrow());

        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let pressed = event.state() == KeyState::Pressed;

        // Stop bind key repeat on any release. This won't work 100% correctly in cases like:
        // 1. Press Mod
        // 2. Press Left (repeat starts)
        // 3. Press PgDown (new repeat starts)
        // 4. Release Left (PgDown repeat stops)
        // But it's good enough for now.
        // FIXME: handle this properly.
        if !pressed && let Some(token) = self.niri.bind_repeat_timer.take() {
            self.niri.event_loop.remove(token);
        }

        if pressed {
            self.hide_cursor_if_needed();
        }

        // Accessibility modifier grabs should override XKB state changes (e.g. Caps Lock), so we
        // need to process them before keyboard.input() below.
        //
        // Other accessibility-grabbed keys should still update our XKB state, but not cause any
        // other changes.
        let _ = consumed_by_a11y;

        let Some(Some(bind)) = self.niri.seat.get_keyboard().unwrap().input(
            self,
            event.key_code(),
            event.state(),
            serial,
            time,
            |this, mods, keysym| {
                let key_code = event.key_code();
                let modified = keysym.modified_sym();
                let raw = keysym.raw_latin_sym_or_raw_current_sym();

                // After updating XKB state from accessibility-grabbed keys, return right away and
                // don't handle them.

                if this.niri.exit_confirm_dialog.is_open() && pressed {
                    if raw == Some(Keysym::Return) {
                        info!("quitting after confirming exit dialog");
                        this.niri.stop_signal.stop();
                    }

                    // Don't send this press to any clients.
                    this.niri.suppressed_keys.insert(key_code);
                    return FilterResult::Intercept(None);
                }

                if pressed && raw == Some(Keysym::Escape) {
                    // Cancel certain grabs on Escape.
                    let pointer = this.niri.seat.get_pointer().unwrap();
                    if pointer
                        .with_grab(|_, grab| Self::grab_can_be_cancelled_with_esc(grab))
                        .unwrap_or(false)
                    {
                        pointer.unset_grab(this, serial, time);
                        this.niri.suppressed_keys.insert(key_code);
                        return FilterResult::Intercept(None);
                    }
                }

                if let Some(Keysym::space) = raw {
                    this.niri.screenshot_ui.set_space_down(pressed);
                }

                let res = {
                    let config = this.niri.config.borrow();
                    let bindings = make_binds_iter(&config);

                    should_intercept_key(
                        &mut this.niri.suppressed_keys,
                        bindings,
                        mod_key,
                        key_code,
                        modified,
                        raw,
                        pressed,
                        *mods,
                        &this.niri.screenshot_ui,
                        this.niri.config.borrow().input.disable_power_key_handling,
                    )
                };

                if matches!(res, FilterResult::Forward) {
                    // If we didn't find any bind, try other hardcoded keys.
                    if this.niri.keyboard_focus.is_overview()
                        && pressed
                        && let Some(bind) = raw.and_then(|raw| hardcoded_overview_bind(raw, *mods))
                    {
                        this.niri.suppressed_keys.insert(key_code);
                        return FilterResult::Intercept(Some(bind));
                    }
                }

                res
            },
        ) else {
            return;
        };

        if !pressed {
            return;
        }

        self.handle_bind(bind.clone());

        self.start_key_repeat(bind);
    }

    fn start_key_repeat(&mut self, bind: Bind) {
        if !bind.repeat {
            return;
        }

        // Stop the previous key repeat if any.
        if let Some(token) = self.niri.bind_repeat_timer.take() {
            self.niri.event_loop.remove(token);
        }

        let config = self.niri.config.borrow();
        let config = &config.input.keyboard;

        let repeat_rate = config.repeat_rate;
        if repeat_rate == 0 {
            return;
        }
        let repeat_duration = Duration::from_secs_f64(1. / f64::from(repeat_rate));

        let repeat_timer =
            Timer::from_duration(Duration::from_millis(u64::from(config.repeat_delay)));

        let token = self
            .niri
            .event_loop
            .insert_source(repeat_timer, move |_, _, state| {
                state.handle_bind(bind.clone());
                TimeoutAction::ToDuration(repeat_duration)
            })
            .unwrap();

        self.niri.bind_repeat_timer = Some(token);
    }

    fn hide_cursor_if_needed(&mut self) {
        // If the pointer is already invisible, don't reset it back to Hidden causing one frame
        // of hover.
        if !self.niri.pointer_visibility.is_visible() {
            return;
        }

        if !self.niri.config.borrow().cursor.hide_when_typing {
            return;
        }

        self.niri.pointer_visibility = PointerVisibility::Hidden;
        self.niri.queue_redraw_all();
    }

    pub fn handle_bind(&mut self, bind: Bind) {
        let Some(cooldown) = bind.cooldown else {
            self.do_action(bind.action, bind.allow_when_locked);
            return;
        };

        // Check this first so that it doesn't trigger the cooldown.
        if self.niri.is_locked() && !(bind.allow_when_locked || allowed_when_locked(&bind.action)) {
            return;
        }

        match self.niri.bind_cooldown_timers.entry(bind.key) {
            // The bind is on cooldown.
            Entry::Occupied(_) => (),
            Entry::Vacant(entry) => {
                let timer = Timer::from_duration(cooldown);
                let token = self
                    .niri
                    .event_loop
                    .insert_source(timer, move |_, _, state| {
                        if state.niri.bind_cooldown_timers.remove(&bind.key).is_none() {
                            error!("bind cooldown timer entry disappeared");
                        }
                        TimeoutAction::Drop
                    })
                    .unwrap();
                entry.insert(token);

                self.do_action(bind.action, bind.allow_when_locked);
            }
        }
    }

    pub fn do_action(&mut self, action: Action, allow_when_locked: bool) {
        if self.niri.is_locked() && !(allow_when_locked || allowed_when_locked(&action)) {
            return;
        }

        match action {
            Action::Quit(skip_confirmation) => {
                if !skip_confirmation && self.niri.exit_confirm_dialog.show() {
                    self.niri.queue_redraw_all();
                    return;
                }

                info!("quitting as requested");
                self.niri.stop_signal.stop()
            }
            Action::ChangeVt(vt) => {
                self.backend.change_vt(vt);
                // Changing VT may not deliver the key releases, so clear the state.
                self.niri.suppressed_keys.clear();
            }
            Action::Suspend => {
                self.backend.suspend();
                // Suspend may not deliver the key releases, so clear the state.
                self.niri.suppressed_keys.clear();
            }
            Action::PowerOffMonitors => {
                self.niri.deactivate_monitors(&mut self.backend);
            }
            Action::PowerOnMonitors => {
                self.niri.activate_monitors(&mut self.backend);
            }
            Action::Spawn(command) => {
                spawn(command);
            }
            Action::SpawnSh(command) => {
                spawn_sh(command);
            }
            Action::DoScreenTransition(delay_ms) => {
                self.backend.with_primary_renderer(|renderer| {
                    self.niri.do_screen_transition(renderer, delay_ms);
                });
            }
            Action::ScreenshotScreen(write_to_disk, show_pointer, path) => {
                let active = self.niri.layout.active_output().cloned();
                if let Some(active) = active {
                    self.backend.with_primary_renderer(|renderer| {
                        if let Err(err) = self.niri.screenshot(
                            renderer,
                            &active,
                            write_to_disk,
                            show_pointer,
                            path,
                        ) {
                            warn!("error taking screenshot: {err:?}");
                        }
                    });
                }
            }
            Action::ConfirmScreenshot { write_to_disk } => {
                self.confirm_screenshot(write_to_disk);
            }
            Action::CancelScreenshot => {
                if !self.niri.screenshot_ui.is_open() {
                    return;
                }

                self.niri.screenshot_ui.close();
                self.niri
                    .cursor_manager
                    .set_cursor_image(CursorImageStatus::default_named());
                self.niri.queue_redraw_all();
            }
            Action::ScreenshotTogglePointer => {
                self.niri.screenshot_ui.toggle_pointer();
                self.niri.queue_redraw_all();
            }
            Action::Screenshot(show_cursor, path) => {
                self.open_screenshot_ui(show_cursor, path);
            }
            Action::ScreenshotWindow(write_to_disk, show_pointer, path) => {
                let focus = self.niri.layout.focus_with_output();
                if let Some((mapped, output)) = focus {
                    self.backend.with_primary_renderer(|renderer| {
                        if let Err(err) = self.niri.screenshot_window(
                            renderer,
                            output,
                            mapped,
                            write_to_disk,
                            show_pointer,
                            path,
                        ) {
                            warn!("error taking screenshot: {err:?}");
                        }
                    });
                }
            }
            Action::ScreenshotWindowById {
                id,
                write_to_disk,
                show_pointer,
                path,
            } => {
                let mut windows = self.niri.layout.windows();
                let window = windows.find(|(_, m)| m.id().get() == id);
                if let Some((Some(monitor), mapped)) = window {
                    let output = monitor.output();
                    self.backend.with_primary_renderer(|renderer| {
                        if let Err(err) = self.niri.screenshot_window(
                            renderer,
                            output,
                            mapped,
                            write_to_disk,
                            show_pointer,
                            path,
                        ) {
                            warn!("error taking screenshot: {err:?}");
                        }
                    });
                }
            }
            Action::CloseWindow => {
                if let Some(mapped) = self.niri.layout.focus() {
                    mapped.toplevel().send_close();
                }
            }
            Action::CloseWindowById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                if let Some((_, mapped)) = window {
                    mapped.toplevel().send_close();
                }
            }
            Action::FullscreenWindow => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.toggle_fullscreen(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FullscreenWindowById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_fullscreen(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleWindowedFullscreen => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.toggle_windowed_fullscreen(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleWindowedFullscreenById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_windowed_fullscreen(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusWindow(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.focus_window(&window);
                }
            }
            Action::FocusWindowInColumn(index) => {
                self.niri.layout.focus_window_in_column(index);
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowPrevious => {
                let current = self.niri.layout.focus().map(|win| win.id());
                if let Some(window) = self
                    .niri
                    .layout
                    .windows()
                    .map(|(_, win)| win)
                    .filter(|win| Some(win.id()) != current)
                    .max_by_key(|win| win.get_focus_timestamp())
                    .map(|win| win.window.clone())
                {
                    self.focus_window(&window);
                }
            }
            Action::SwitchLayout(action) => {
                let keyboard = &self.niri.seat.get_keyboard().unwrap();
                keyboard.with_xkb_state(self, |mut state| match action {
                    LayoutSwitchTarget::Next => state.cycle_next_layout(),
                    LayoutSwitchTarget::Prev => state.cycle_prev_layout(),
                    LayoutSwitchTarget::Index(layout) => {
                        let num_layouts = state.xkb().lock().unwrap().layouts().count();
                        if usize::from(layout) >= num_layouts {
                            warn!("requested layout doesn't exist")
                        } else {
                            state.set_layout(Layout(layout.into()))
                        }
                    }
                });
            }
            Action::MoveColumnLeft => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_left();
                } else {
                    self.niri.layout.move_left();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnRight => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_right();
                } else {
                    self.niri.layout.move_right();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnToFirst => {
                self.niri.layout.move_column_to_first();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnToLast => {
                self.niri.layout.move_column_to_last();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnLeftOrToMonitorLeft => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_left();
                } else if let Some(output) = self.niri.output_left() {
                    if self.niri.layout.move_column_left_or_to_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.move_left();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnRightOrToMonitorRight => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_right();
                } else if let Some(output) = self.niri.output_right() {
                    if self.niri.layout.move_column_right_or_to_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.move_right();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowDown => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_down();
                } else {
                    self.niri.layout.move_down();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowUp => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_up();
                } else {
                    self.niri.layout.move_up();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowDownOrToWorkspaceDown => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_down();
                } else {
                    self.niri.layout.move_down_or_to_workspace_down();
                    self.maybe_warp_cursor_to_focus();
                }
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowUpOrToWorkspaceUp => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_up();
                } else {
                    self.niri.layout.move_up_or_to_workspace_up();
                    self.maybe_warp_cursor_to_focus();
                }
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ConsumeOrExpelWindowLeft => {
                self.niri.layout.consume_or_expel_window_left(None);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ConsumeOrExpelWindowLeftById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.consume_or_expel_window_left(Some(&window));
                    self.maybe_warp_cursor_to_focus();
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ConsumeOrExpelWindowRight => {
                self.niri.layout.consume_or_expel_window_right(None);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ConsumeOrExpelWindowRightById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri
                        .layout
                        .consume_or_expel_window_right(Some(&window));
                    self.maybe_warp_cursor_to_focus();
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusColumnLeft => {
                self.niri.layout.focus_left();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnLeftUnderMouse => {
                if let Some((output, ws)) = self.niri.workspace_under_cursor(true) {
                    let ws_id = ws.id();
                    let ws = {
                        let mut workspaces = self.niri.layout.workspaces_mut();
                        workspaces.find(|ws| ws.id() == ws_id).unwrap()
                    };
                    ws.focus_left();
                    self.maybe_warp_cursor_to_focus();
                    self.niri.layer_shell_on_demand_focus = None;
                    self.niri.queue_redraw(&output);
                }
            }
            Action::FocusColumnRight => {
                self.niri.layout.focus_right();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnRightUnderMouse => {
                if let Some((output, ws)) = self.niri.workspace_under_cursor(true) {
                    let ws_id = ws.id();
                    let ws = {
                        let mut workspaces = self.niri.layout.workspaces_mut();
                        workspaces.find(|ws| ws.id() == ws_id).unwrap()
                    };
                    ws.focus_right();
                    self.maybe_warp_cursor_to_focus();
                    self.niri.layer_shell_on_demand_focus = None;
                    self.niri.queue_redraw(&output);
                }
            }
            Action::FocusColumnFirst => {
                self.niri.layout.focus_column_first();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnLast => {
                self.niri.layout.focus_column_last();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnRightOrFirst => {
                self.niri.layout.focus_column_right_or_first();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnLeftOrLast => {
                self.niri.layout.focus_column_left_or_last();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumn(index) => {
                self.niri.layout.focus_column(index);
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowOrMonitorUp => {
                if let Some(output) = self.niri.output_up() {
                    if self.niri.layout.focus_window_up_or_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.focus_up();
                    self.maybe_warp_cursor_to_focus();
                }
                self.niri.layer_shell_on_demand_focus = None;

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowOrMonitorDown => {
                if let Some(output) = self.niri.output_down() {
                    if self.niri.layout.focus_window_down_or_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.focus_down();
                    self.maybe_warp_cursor_to_focus();
                }
                self.niri.layer_shell_on_demand_focus = None;

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnOrMonitorLeft => {
                if let Some(output) = self.niri.output_left() {
                    if self.niri.layout.focus_column_left_or_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.focus_left();
                    self.maybe_warp_cursor_to_focus();
                }
                self.niri.layer_shell_on_demand_focus = None;

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnOrMonitorRight => {
                if let Some(output) = self.niri.output_right() {
                    if self.niri.layout.focus_column_right_or_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.focus_right();
                    self.maybe_warp_cursor_to_focus();
                }
                self.niri.layer_shell_on_demand_focus = None;

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowDown => {
                self.niri.layout.focus_down();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowUp => {
                self.niri.layout.focus_up();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowDownOrColumnLeft => {
                self.niri.layout.focus_down_or_left();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowDownOrColumnRight => {
                self.niri.layout.focus_down_or_right();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowUpOrColumnLeft => {
                self.niri.layout.focus_up_or_left();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowUpOrColumnRight => {
                self.niri.layout.focus_up_or_right();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowOrWorkspaceDown => {
                self.niri.layout.focus_window_or_workspace_down();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowOrWorkspaceUp => {
                self.niri.layout.focus_window_or_workspace_up();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowTop => {
                self.niri.layout.focus_window_top();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowBottom => {
                self.niri.layout.focus_window_bottom();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowDownOrTop => {
                self.niri.layout.focus_window_down_or_top();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowUpOrBottom => {
                self.niri.layout.focus_window_up_or_bottom();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToWorkspaceDown(focus) => {
                self.niri.layout.move_to_workspace_down(focus);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToWorkspaceUp(focus) => {
                self.niri.layout.move_to_workspace_up(focus);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToWorkspace(reference, focus) => {
                if let Some((mut output, index)) =
                    self.niri.find_output_and_workspace_index(reference)
                {
                    // The source output is always the active output, so if the target output is
                    // also the active output, we don't need to use move_to_output().
                    if let Some(active) = self.niri.layout.active_output()
                        && output.as_ref() == Some(active)
                    {
                        output = None;
                    }

                    let activate = if focus {
                        ActivateWindow::Smart
                    } else {
                        ActivateWindow::No
                    };

                    if let Some(output) = output {
                        self.niri
                            .layout
                            .move_to_output(None, &output, Some(index), activate);

                        if focus {
                            if !self.maybe_warp_cursor_to_focus_centered() {
                                self.move_cursor_to_output(&output);
                            }
                        } else {
                            self.maybe_warp_cursor_to_focus();
                        }
                    } else {
                        self.niri.layout.move_to_workspace(None, index, activate);
                        self.maybe_warp_cursor_to_focus();
                    }

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveWindowToWorkspaceById {
                window_id: id,
                reference,
                focus,
            } => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window
                    && let Some((output, index)) =
                        self.niri.find_output_and_workspace_index(reference)
                {
                    let target_was_active = self
                        .niri
                        .layout
                        .active_output()
                        .is_some_and(|active| output.as_ref() == Some(active));

                    let activate = if focus {
                        ActivateWindow::Smart
                    } else {
                        ActivateWindow::No
                    };

                    if let Some(output) = output {
                        self.niri.layout.move_to_output(
                            Some(&window),
                            &output,
                            Some(index),
                            activate,
                        );

                        // If the active output changed (window was moved and focused).
                        #[allow(clippy::collapsible_if)]
                        if !target_was_active && self.niri.layout.active_output() == Some(&output) {
                            if !self.maybe_warp_cursor_to_focus_centered() {
                                self.move_cursor_to_output(&output);
                            }
                        }
                    } else {
                        self.niri
                            .layout
                            .move_to_workspace(Some(&window), index, activate);

                        // If we focused the target window.
                        let new_focus = self.niri.layout.focus();
                        if new_focus.is_some_and(|win| win.window == window) {
                            self.maybe_warp_cursor_to_focus();
                        }
                    }

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveColumnToWorkspaceDown(focus) => {
                self.niri.layout.move_column_to_workspace_down(focus);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnToWorkspaceUp(focus) => {
                self.niri.layout.move_column_to_workspace_up(focus);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnToWorkspace(reference, focus) => {
                if let Some((mut output, index)) =
                    self.niri.find_output_and_workspace_index(reference)
                {
                    if let Some(active) = self.niri.layout.active_output()
                        && output.as_ref() == Some(active)
                    {
                        output = None;
                    }

                    if let Some(output) = output {
                        self.niri
                            .layout
                            .move_column_to_output(&output, Some(index), focus);
                        if focus && !self.maybe_warp_cursor_to_focus_centered() {
                            self.move_cursor_to_output(&output);
                        }
                    } else {
                        self.niri.layout.move_column_to_workspace(index, focus);
                        if focus {
                            self.maybe_warp_cursor_to_focus();
                        }
                    }

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveColumnToIndex(idx) => {
                self.niri.layout.move_column_to_index(idx);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWorkspaceDown => {
                self.niri.layout.switch_workspace_down();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWorkspaceDownUnderMouse => {
                if let Some(output) = self.niri.output_under_cursor()
                    && let Some(mon) = self.niri.layout.monitor_for_output_mut(&output)
                {
                    mon.switch_workspace_down();
                    self.maybe_warp_cursor_to_focus();
                    self.niri.layer_shell_on_demand_focus = None;
                    self.niri.queue_redraw(&output);
                }
            }
            Action::FocusWorkspaceUp => {
                self.niri.layout.switch_workspace_up();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWorkspaceUpUnderMouse => {
                if let Some(output) = self.niri.output_under_cursor()
                    && let Some(mon) = self.niri.layout.monitor_for_output_mut(&output)
                {
                    mon.switch_workspace_up();
                    self.maybe_warp_cursor_to_focus();
                    self.niri.layer_shell_on_demand_focus = None;
                    self.niri.queue_redraw(&output);
                }
            }
            Action::FocusWorkspace(reference) => {
                if let Some((mut output, index)) =
                    self.niri.find_output_and_workspace_index(reference)
                {
                    if let Some(active) = self.niri.layout.active_output()
                        && output.as_ref() == Some(active)
                    {
                        output = None;
                    }

                    if let Some(output) = output {
                        self.niri.layout.focus_output(&output);
                        self.niri.layout.switch_workspace(index);
                        if !self.maybe_warp_cursor_to_focus_centered() {
                            self.move_cursor_to_output(&output);
                        }
                    } else {
                        let config = &self.niri.config;
                        if config.borrow().input.workspace_auto_back_and_forth {
                            self.niri.layout.switch_workspace_auto_back_and_forth(index);
                        } else {
                            self.niri.layout.switch_workspace(index);
                        }
                        self.maybe_warp_cursor_to_focus();
                    }
                    self.niri.layer_shell_on_demand_focus = None;

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusWorkspacePrevious => {
                self.niri.layout.switch_workspace_previous();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWorkspaceDown => {
                self.niri.layout.move_workspace_down();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWorkspaceUp => {
                self.niri.layout.move_workspace_up();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWorkspaceToIndex(new_idx) => {
                let new_idx = new_idx.saturating_sub(1);
                self.niri.layout.move_workspace_to_idx(None, new_idx);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWorkspaceToIndexByRef { new_idx, reference } => {
                if let Some(res) = self.niri.find_output_and_workspace_index(reference) {
                    let new_idx = new_idx.saturating_sub(1);
                    self.niri.layout.move_workspace_to_idx(Some(res), new_idx);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::SetWorkspaceName(name) => {
                self.niri.layout.set_workspace_name(name, None);
            }
            Action::SetWorkspaceNameByRef { name, reference } => {
                self.niri.layout.set_workspace_name(name, Some(reference));
            }
            Action::UnsetWorkspaceName => {
                self.niri.layout.unset_workspace_name(None);
            }
            Action::UnsetWorkSpaceNameByRef(reference) => {
                self.niri.layout.unset_workspace_name(Some(reference));
            }
            Action::ConsumeWindowIntoColumn => {
                self.niri.layout.consume_into_column();
                // This does not cause immediate focus or window size change, so warping mouse to
                // focus won't do anything here.
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ExpelWindowFromColumn => {
                self.niri.layout.expel_from_column();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SwapWindowRight => {
                self.niri
                    .layout
                    .swap_window_in_direction(ScrollDirection::Right);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SwapWindowLeft => {
                self.niri
                    .layout
                    .swap_window_in_direction(ScrollDirection::Left);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ToggleColumnTabbedDisplay => {
                self.niri.layout.toggle_column_tabbed_display();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SetColumnDisplay(display) => {
                self.niri.layout.set_column_display(display);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SwitchPresetColumnWidth => {
                self.niri.layout.toggle_width(true);
            }
            Action::SwitchPresetColumnWidthBack => {
                self.niri.layout.toggle_width(false);
            }
            Action::SwitchPresetWindowWidth => {
                self.niri.layout.toggle_window_width(None, true);
            }
            Action::SwitchPresetWindowWidthBack => {
                self.niri.layout.toggle_window_width(None, false);
            }
            Action::SwitchPresetWindowWidthById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_width(Some(&window), true);
                }
            }
            Action::SwitchPresetWindowWidthBackById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_width(Some(&window), false);
                }
            }
            Action::SwitchPresetWindowHeight => {
                self.niri.layout.toggle_window_height(None, true);
            }
            Action::SwitchPresetWindowHeightBack => {
                self.niri.layout.toggle_window_height(None, false);
            }
            Action::SwitchPresetWindowHeightById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_height(Some(&window), true);
                }
            }
            Action::SwitchPresetWindowHeightBackById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_height(Some(&window), false);
                }
            }
            Action::CenterColumn => {
                self.niri.layout.center_column();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::CenterWindow => {
                self.niri.layout.center_window(None);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::CenterWindowById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.center_window(Some(&window));
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::CenterVisibleColumns => {
                self.niri.layout.center_visible_columns();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MaximizeColumn => {
                self.niri.layout.toggle_full_width();
            }
            Action::MaximizeWindowToEdges => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.toggle_maximized(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MaximizeWindowToEdgesById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_maximized(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusMonitorLeft => {
                if let Some(output) = self.niri.output_left() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorRight => {
                if let Some(output) = self.niri.output_right() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorDown => {
                if let Some(output) = self.niri.output_down() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorUp => {
                if let Some(output) = self.niri.output_up() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorPrevious => {
                if let Some(output) = self.niri.output_previous() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorNext => {
                if let Some(output) = self.niri.output_next() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitor(output) => {
                if let Some(output) = self.niri.output_by_name_match(&output).cloned() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::MoveWindowToMonitorLeft => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_left_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_left() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorRight => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_right_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_right() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorDown => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_down_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_down() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorUp => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_up_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_up() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorPrevious => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_previous_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_previous() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorNext => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_next_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_next() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitor(output) => {
                if let Some(output) = self.niri.output_by_name_match(&output).cloned() {
                    if self.niri.screenshot_ui.is_open() {
                        self.move_cursor_to_output(&output);
                        self.niri.screenshot_ui.move_to_output(output);
                    } else {
                        self.niri
                            .layout
                            .move_to_output(None, &output, None, ActivateWindow::Smart);
                        self.niri.layout.focus_output(&output);
                        if !self.maybe_warp_cursor_to_focus_centered() {
                            self.move_cursor_to_output(&output);
                        }
                    }
                }
            }
            Action::MoveWindowToMonitorById { id, output } => {
                if let Some(output) = self.niri.output_by_name_match(&output).cloned() {
                    let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                    let window = window.map(|(_, m)| m.window.clone());

                    if let Some(window) = window {
                        let target_was_active = self
                            .niri
                            .layout
                            .active_output()
                            .is_some_and(|active| output == *active);

                        self.niri.layout.move_to_output(
                            Some(&window),
                            &output,
                            None,
                            ActivateWindow::Smart,
                        );

                        // If the active output changed (window was moved and focused).
                        #[allow(clippy::collapsible_if)]
                        if !target_was_active && self.niri.layout.active_output() == Some(&output) {
                            if !self.maybe_warp_cursor_to_focus_centered() {
                                self.move_cursor_to_output(&output);
                            }
                        }
                    }
                }
            }
            Action::MoveColumnToMonitorLeft => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_left_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_left() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorRight => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_right_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_right() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorDown => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_down_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_down() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorUp => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_up_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_up() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorPrevious => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_previous_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_previous() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorNext => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_next_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_next() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitor(output) => {
                if let Some(output) = self.niri.output_by_name_match(&output).cloned() {
                    if self.niri.screenshot_ui.is_open() {
                        self.move_cursor_to_output(&output);
                        self.niri.screenshot_ui.move_to_output(output);
                    } else {
                        self.niri.layout.move_column_to_output(&output, None, true);
                        self.niri.layout.focus_output(&output);
                        if !self.maybe_warp_cursor_to_focus_centered() {
                            self.move_cursor_to_output(&output);
                        }
                    }
                }
            }
            Action::SetColumnWidth(change) => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.set_width(change);

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                } else {
                    self.niri.layout.set_column_width(change);
                }
            }
            Action::SetWindowWidth(change) => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.set_width(change);

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                } else {
                    self.niri.layout.set_window_width(None, change);
                }
            }
            Action::SetWindowWidthById { id, change } => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.set_window_width(Some(&window), change);
                }
            }
            Action::SetWindowHeight(change) => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.set_height(change);

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                } else {
                    self.niri.layout.set_window_height(None, change);
                }
            }
            Action::SetWindowHeightById { id, change } => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.set_window_height(Some(&window), change);
                }
            }
            Action::ResetWindowHeight => {
                self.niri.layout.reset_window_height(None);
            }
            Action::ResetWindowHeightById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.reset_window_height(Some(&window));
                }
            }
            Action::ExpandColumnToAvailableWidth => {
                self.niri.layout.expand_column_to_available_width();
            }
            Action::ShowHotkeyOverlay => {
                if self.niri.hotkey_overlay.show() {
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveWorkspaceToMonitorLeft => {
                if let Some(output) = self.niri.output_left() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorRight => {
                if let Some(output) = self.niri.output_right() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorDown => {
                if let Some(output) = self.niri.output_down() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorUp => {
                if let Some(output) = self.niri.output_up() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorPrevious => {
                if let Some(output) = self.niri.output_previous() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorNext => {
                if let Some(output) = self.niri.output_next() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitor(new_output) => {
                if let Some(new_output) = self.niri.output_by_name_match(&new_output).cloned()
                    && self.niri.layout.move_workspace_to_output(&new_output)
                    && !self.maybe_warp_cursor_to_focus_centered()
                {
                    self.move_cursor_to_output(&new_output);
                }
            }
            Action::MoveWorkspaceToMonitorByRef {
                output_name,
                reference,
            } => {
                if let Some((output, old_idx)) =
                    self.niri.find_output_and_workspace_index(reference)
                    && let Some(new_output) = self.niri.output_by_name_match(&output_name).cloned()
                    && self
                        .niri
                        .layout
                        .move_workspace_to_output_by_id(old_idx, output, &new_output)
                {
                    // Cursor warp already calls `queue_redraw_all`
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&new_output);
                    }
                }
            }
            Action::ToggleWindowFloating => {
                self.niri.layout.toggle_window_floating(None);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ToggleWindowFloatingById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_floating(Some(&window));
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveWindowToFloating => {
                self.niri.layout.set_window_floating(None, true);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToFloatingById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.set_window_floating(Some(&window), true);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveWindowToTiling => {
                self.niri.layout.set_window_floating(None, false);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToTilingById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.set_window_floating(Some(&window), false);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusFloating => {
                self.niri.layout.focus_floating();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusTiling => {
                self.niri.layout.focus_tiling();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SwitchFocusBetweenFloatingAndTiling => {
                self.niri.layout.switch_focus_floating_tiling();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveFloatingWindowById { id, x, y } => {
                let window = if let Some(id) = id {
                    let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                    let window = window.map(|(_, m)| m.window.clone());
                    if window.is_none() {
                        return;
                    }
                    window
                } else {
                    None
                };

                self.niri
                    .layout
                    .move_floating_window(window.as_ref(), x, y, true);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ToggleWindowRuleOpacity => {
                let active_window = self
                    .niri
                    .layout
                    .active_workspace_mut()
                    .and_then(|ws| ws.active_window_mut());
                if let Some(window) = active_window
                    && window.rules().opacity.is_some_and(|o| o != 1.)
                {
                    window.toggle_ignore_opacity_window_rule();
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleWindowRuleOpacityById(id) => {
                let window = self
                    .niri
                    .layout
                    .workspaces_mut()
                    .find_map(|ws| ws.windows_mut().find(|w| w.id().get() == id));
                if let Some(window) = window
                    && window.rules().opacity.is_some_and(|o| o != 1.)
                {
                    window.toggle_ignore_opacity_window_rule();
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleOverview => {
                self.niri.layout.toggle_overview();
                self.niri.queue_redraw_all();
            }
            Action::OpenOverview => {
                if self.niri.layout.open_overview() {
                    self.niri.queue_redraw_all();
                }
            }
            Action::CloseOverview => {
                if self.niri.layout.close_overview() {
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleWindowUrgent(id) => {
                let window = self
                    .niri
                    .layout
                    .workspaces_mut()
                    .find_map(|ws| ws.windows_mut().find(|w| w.id().get() == id));
                if let Some(window) = window {
                    let urgent = window.is_urgent();
                    window.set_urgent(!urgent);
                }
                self.niri.queue_redraw_all();
            }
            Action::SetWindowUrgent(id) => {
                let window = self
                    .niri
                    .layout
                    .workspaces_mut()
                    .find_map(|ws| ws.windows_mut().find(|w| w.id().get() == id));
                if let Some(window) = window {
                    window.set_urgent(true);
                }
                self.niri.queue_redraw_all();
            }
            Action::UnsetWindowUrgent(id) => {
                let window = self
                    .niri
                    .layout
                    .workspaces_mut()
                    .find_map(|ws| ws.windows_mut().find(|w| w.id().get() == id));
                if let Some(window) = window {
                    window.set_urgent(false);
                }
                self.niri.queue_redraw_all();
            }
            Action::LoadConfigFile(path) => {
                if let Some(watcher) = &self.niri.config_file_watcher {
                    watcher.load_config(path);
                }
            }
        }
    }

    fn on_pointer_motion<I: InputBackend>(&mut self, event: I::PointerMotionEvent) {
        let was_inside_hot_corner = self.niri.pointer_inside_hot_corner;
        // Any of the early returns here mean that the pointer is not inside the hot corner.
        self.niri.pointer_inside_hot_corner = false;

        // We need an output to be able to move the pointer.
        if self.niri.global_space.outputs().next().is_none() {
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();

        let pointer = self.niri.seat.get_pointer().unwrap();

        let pos = pointer.current_location();

        // We have an output, so we can compute the new location and focus.
        let mut new_pos = pos + event.delta();

        // We received an event for the regular pointer, so show it now.
        self.niri.pointer_visibility = PointerVisibility::Visible;

        // Check if we have an active pointer constraint.
        //
        // FIXME: ideally this should use the pointer focus with up-to-date global location.
        let mut pointer_confined = None;
        if let Some(under) = &self.niri.pointer_contents.surface {
            // No need to check if the pointer focus surface matches, because here we're checking
            // for an already-active constraint, and the constraint is deactivated when the focused
            // surface changes.
            let pos_within_surface = pos - under.1;

            let mut pointer_locked = false;
            with_pointer_constraint(&under.0, &pointer, |constraint| {
                let Some(constraint) = constraint else { return };
                if !constraint.is_active() {
                    return;
                }

                // Constraint does not apply if not within region.
                if let Some(region) = constraint.region()
                    && !region.contains(pos_within_surface.to_i32_round())
                {
                    return;
                }

                match &*constraint {
                    PointerConstraint::Locked(_locked) => {
                        pointer_locked = true;
                    }
                    PointerConstraint::Confined(confine) => {
                        pointer_confined = Some((under.clone(), confine.region().cloned()));
                    }
                }
            });

            // If the pointer is locked, only send relative motion.
            if pointer_locked {
                pointer.relative_motion(
                    self,
                    Some(under.clone()),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                pointer.frame(self);

                return;
            }
        }

        // Warp pointer across the screen during the spatial movement grabs.
        let spatial_grab = pointer.with_grab(|_, grab| {
            let grab = grab.as_any();
            if let Some(grab) = grab.downcast_ref::<SpatialMovementGrab>() {
                if let Some(output) = grab.view_offset_output() {
                    return Some((output.clone(), true));
                } else if let Some(output) = grab.workspace_switch_output() {
                    return Some((output.clone(), false));
                }
            } else if let Some(grab) = grab.downcast_ref::<MoveGrab>()
                && let Some(output) = grab.view_offset_output()
            {
                return Some((output.clone(), true));
            }
            None
        });
        if let Some((output, horizontal)) = spatial_grab.flatten()
            && let Some(geo) = self.niri.global_space.output_geometry(&output)
        {
            let geo = geo.to_f64();
            if horizontal {
                new_pos.x = (new_pos.x - geo.loc.x).rem_euclid(geo.size.w) + geo.loc.x;
                new_pos.y = new_pos.y.clamp(geo.loc.y, geo.loc.y + geo.size.h - 1.);
            } else {
                new_pos.x = new_pos.x.clamp(geo.loc.x, geo.loc.x + geo.size.w - 1.);
                new_pos.y = (new_pos.y - geo.loc.y).rem_euclid(geo.size.h) + geo.loc.y;
            }
        }

        if self
            .niri
            .global_space
            .output_under(new_pos)
            .next()
            .is_none()
        {
            // We ended up outside the outputs and need to clip the movement.
            if let Some(output) = self.niri.global_space.output_under(pos).next() {
                // The pointer was previously on some output. Clip the movement against its
                // boundaries.
                let geom = self.niri.global_space.output_geometry(output).unwrap();
                new_pos.x = new_pos
                    .x
                    .clamp(geom.loc.x as f64, (geom.loc.x + geom.size.w - 1) as f64);
                new_pos.y = new_pos
                    .y
                    .clamp(geom.loc.y as f64, (geom.loc.y + geom.size.h - 1) as f64);
            } else {
                // The pointer was not on any output in the first place. Find one for it.
                // Let's do the simple thing and just put it on the first output.
                let output = self.niri.global_space.outputs().next().unwrap();
                let geom = self.niri.global_space.output_geometry(output).unwrap();
                new_pos = center(geom).to_f64();
            }
        }

        if let Some(output) = self.niri.screenshot_ui.selection_output() {
            let geom = self.niri.global_space.output_geometry(output).unwrap();
            let point = (new_pos - geom.loc.to_f64())
                .to_physical(output.current_scale().fractional_scale())
                .to_i32_round::<i32>();

            self.niri.screenshot_ui.pointer_motion(point, None);
        }

        let under = self.niri.contents_under(new_pos);

        // Handle confined pointer.
        if let Some((focus_surface, region)) = pointer_confined {
            let mut prevent = false;

            // Prevent the pointer from leaving the focused surface.
            if Some(&focus_surface.0) != under.surface.as_ref().map(|(s, _)| s) {
                prevent = true;
            }

            // Prevent the pointer from leaving the confine region, if any.
            if let Some(region) = region {
                let new_pos_within_surface = new_pos - focus_surface.1;
                if !region.contains(new_pos_within_surface.to_i32_round()) {
                    prevent = true;
                }
            }

            if prevent {
                pointer.relative_motion(
                    self,
                    Some(focus_surface),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                pointer.frame(self);

                return;
            }
        }

        self.niri.handle_focus_follows_mouse(&under);

        self.niri.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface.clone(),
            &MotionEvent {
                location: new_pos,
                serial,
                time: event.time_msec(),
            },
        );

        pointer.relative_motion(
            self,
            under.surface,
            &RelativeMotionEvent {
                delta: event.delta(),
                delta_unaccel: event.delta_unaccel(),
                utime: event.time(),
            },
        );

        pointer.frame(self);

        // contents_under() will return no surface when the hot corner should trigger, so
        // pointer.motion() will set the current focus to None.
        if under.hot_corner && pointer.current_focus().is_none() {
            if !was_inside_hot_corner
                && pointer
                    .with_grab(|_, grab| grab_allows_hot_corner(grab))
                    .unwrap_or(true)
            {
                self.niri.layout.toggle_overview();
            }
            self.niri.pointer_inside_hot_corner = true;
        }

        // Activate a new confinement if necessary.
        self.niri.maybe_activate_pointer_constraint();

        // Inform the layout of an ongoing DnD operation.
        let is_dnd_grab = pointer
            .with_grab(|_, grab| Self::is_dnd_grab(grab.as_any()))
            .unwrap_or(false);
        if is_dnd_grab && let Some((output, pos_within_output)) = self.niri.output_under(new_pos) {
            let output = output.clone();
            self.niri.layout.dnd_update(output, pos_within_output);
        }

        // Redraw to update the cursor position.
        // FIXME: redraw only outputs overlapping the cursor.
        self.niri.queue_redraw_all();
    }

    fn on_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        event: I::PointerMotionAbsoluteEvent,
    ) {
        let was_inside_hot_corner = self.niri.pointer_inside_hot_corner;
        // Any of the early returns here mean that the pointer is not inside the hot corner.
        self.niri.pointer_inside_hot_corner = false;

        let Some(pos) = self.compute_absolute_location(&event, None).or_else(|| {
            self.global_bounding_rectangle().map(|output_geo| {
                event.position_transformed(output_geo.size) + output_geo.loc.to_f64()
            })
        }) else {
            return;
        };

        let serial = SERIAL_COUNTER.next_serial();

        let pointer = self.niri.seat.get_pointer().unwrap();

        if let Some(output) = self.niri.screenshot_ui.selection_output() {
            let geom = self.niri.global_space.output_geometry(output).unwrap();
            let point = (pos - geom.loc.to_f64())
                .to_physical(output.current_scale().fractional_scale())
                .to_i32_round::<i32>();

            self.niri.screenshot_ui.pointer_motion(point, None);
        }

        let under = self.niri.contents_under(pos);

        self.niri.handle_focus_follows_mouse(&under);

        self.niri.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface,
            &MotionEvent {
                location: pos,
                serial,
                time: event.time_msec(),
            },
        );

        pointer.frame(self);

        // contents_under() will return no surface when the hot corner should trigger, so
        // pointer.motion() will set the current focus to None.
        if under.hot_corner && pointer.current_focus().is_none() {
            if !was_inside_hot_corner
                && pointer
                    .with_grab(|_, grab| grab_allows_hot_corner(grab))
                    .unwrap_or(true)
            {
                self.niri.layout.toggle_overview();
            }
            self.niri.pointer_inside_hot_corner = true;
        }

        self.niri.maybe_activate_pointer_constraint();

        // We moved the pointer, show it.
        self.niri.pointer_visibility = PointerVisibility::Visible;

        // Inform the layout of an ongoing DnD operation.
        let is_dnd_grab = pointer
            .with_grab(|_, grab| Self::is_dnd_grab(grab.as_any()))
            .unwrap_or(false);
        if is_dnd_grab && let Some((output, pos_within_output)) = self.niri.output_under(pos) {
            let output = output.clone();
            self.niri.layout.dnd_update(output, pos_within_output);
        }

        // Redraw to update the cursor position.
        // FIXME: redraw only outputs overlapping the cursor.
        self.niri.queue_redraw_all();
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        let pointer = self.niri.seat.get_pointer().unwrap();

        let serial = SERIAL_COUNTER.next_serial();

        let button = event.button();

        let button_code = event.button_code();

        let button_state = event.state();

        let mod_key = self.backend.mod_key(&self.niri.config.borrow());

        // Ignore release events for mouse clicks that triggered a bind.
        if self.niri.suppressed_buttons.remove(&button_code) {
            return;
        }

        let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
        let modifiers = modifiers_from_state(mods);
        let mod_down = modifiers.contains(mod_key.to_modifiers());

        if ButtonState::Pressed == button_state {
            if self.niri.mods_with_mouse_binds.contains(&modifiers)
                && let Some(bind) = match button {
                    Some(MouseButton::Left) => Some(Trigger::MouseLeft),
                    Some(MouseButton::Right) => Some(Trigger::MouseRight),
                    Some(MouseButton::Middle) => Some(Trigger::MouseMiddle),
                    Some(MouseButton::Back) => Some(Trigger::MouseBack),
                    Some(MouseButton::Forward) => Some(Trigger::MouseForward),
                    _ => None,
                }
                .and_then(|trigger| {
                    let config = self.niri.config.borrow();
                    let bindings = make_binds_iter(&config);
                    find_configured_bind(bindings, mod_key, trigger, mods)
                })
                .filter(|bind| {
                    !self.niri.screenshot_ui.is_open() || allowed_during_screenshot(&bind.action)
                })
            {
                self.niri.suppressed_buttons.insert(button_code);
                self.handle_bind(bind.clone());
                return;
            };

            // We received an event for the regular pointer, so show it now.
            self.niri.pointer_visibility = PointerVisibility::Visible;

            let is_overview_open = self.niri.layout.is_overview_open();

            if is_overview_open
                && !pointer.is_grabbed()
                && button == Some(MouseButton::Right)
                && let Some((output, ws)) = self.niri.workspace_under_cursor(true)
            {
                let ws_id = ws.id();
                let ws_idx = self.niri.layout.find_workspace_by_id(ws_id).unwrap().0;

                self.niri.layout.focus_output(&output);

                let location = pointer.current_location();
                let start_data = PointerGrabStartData {
                    focus: None,
                    button: button_code,
                    location,
                };
                self.niri
                    .layout
                    .view_offset_gesture_begin(&output, Some(ws_idx));
                let grab = SpatialMovementGrab::new(start_data, output, ws_id, true);
                pointer.set_grab(self, grab, serial, Focus::Clear);
                self.niri
                    .cursor_manager
                    .set_cursor_image(CursorImageStatus::Named(CursorIcon::AllScroll));

                // FIXME: granular.
                self.niri.queue_redraw_all();
                return;
            }

            if button == Some(MouseButton::Middle) && !pointer.is_grabbed() && mod_down {
                let output_ws = if is_overview_open {
                    self.niri.workspace_under_cursor(true)
                } else {
                    // We don't want to accidentally "catch" the wrong workspace during
                    // animations.
                    self.niri.output_under_cursor().and_then(|output| {
                        let mon = self.niri.layout.monitor_for_output(&output)?;
                        Some((output, mon.active_workspace_ref()))
                    })
                };

                if let Some((output, ws)) = output_ws {
                    let ws_id = ws.id();

                    self.niri.layout.focus_output(&output);

                    let location = pointer.current_location();
                    let start_data = PointerGrabStartData {
                        focus: None,
                        button: button_code,
                        location,
                    };
                    let grab = SpatialMovementGrab::new(start_data, output, ws_id, false);
                    pointer.set_grab(self, grab, serial, Focus::Clear);
                    self.niri
                        .cursor_manager
                        .set_cursor_image(CursorImageStatus::Named(CursorIcon::AllScroll));

                    // FIXME: granular.
                    self.niri.queue_redraw_all();

                    // Don't activate the window under the cursor to avoid unnecessary
                    // scrolling when e.g. Mod+MMB clicking on a partially off-screen window.
                    return;
                }
            }

            if let Some(mapped) = self.niri.window_under_cursor() {
                let window = mapped.window.clone();

                // Check if we need to start an interactive move.
                if button == Some(MouseButton::Left) && !pointer.is_grabbed() {
                    if is_overview_open || mod_down {
                        let location = pointer.current_location();

                        if !is_overview_open {
                            self.niri.layout.activate_window(&window);
                        }

                        let start_data = PointerGrabStartData {
                            focus: None,
                            button: button_code,
                            location,
                        };
                        let icon = CursorIcon::Grabbing;
                        if let Some(grab) =
                            MoveGrab::new(self, start_data, window.clone(), false, Some(icon))
                        {
                            pointer.set_grab(self, grab, serial, Focus::Clear);

                            // Set the cursor to Grabbing right away for Mod+LMB since it doesn't
                            // do any other gesture.
                            //
                            // In the overview, we click to activate window and close the overview,
                            // in this case setting the cursor right away would be distracting.
                            if !is_overview_open {
                                self.niri
                                    .cursor_manager
                                    .set_cursor_image(CursorImageStatus::Named(icon));
                            }
                        }
                    }
                }
                // Check if we need to start an interactive resize.
                else if button == Some(MouseButton::Right) && !pointer.is_grabbed() && mod_down {
                    let location = pointer.current_location();
                    let (output, pos_within_output) = self.niri.output_under(location).unwrap();
                    let edges = self
                        .niri
                        .layout
                        .resize_edges_under(output, pos_within_output)
                        .unwrap_or(ResizeEdge::empty());

                    if !edges.is_empty() {
                        // See if we got a double resize-click gesture.
                        // FIXME: deduplicate with resize_request in xdg-shell somehow.
                        let time = get_monotonic_time();
                        let last_cell = mapped.last_interactive_resize_start();
                        let mut last = last_cell.get();
                        last_cell.set(Some((time, edges)));

                        // Floating windows don't have either of the double-resize-click
                        // gestures, so just allow it to resize.
                        if mapped.is_floating() {
                            last = None;
                            last_cell.set(None);
                        }

                        if let Some((last_time, last_edges)) = last
                            && time.saturating_sub(last_time) <= DOUBLE_CLICK_TIME
                        {
                            // Allow quick resize after a triple click.
                            last_cell.set(None);

                            let intersection = edges.intersection(last_edges);
                            if intersection.intersects(ResizeEdge::LEFT_RIGHT) {
                                // FIXME: don't activate once we can pass specific windows
                                // to actions.
                                self.niri.layout.activate_window(&window);
                                self.niri.layout.toggle_full_width();
                            }
                            if intersection.intersects(ResizeEdge::TOP_BOTTOM) {
                                self.niri.layout.activate_window(&window);
                                self.niri.layout.reset_window_height(Some(&window));
                            }
                            // FIXME: granular.
                            self.niri.queue_redraw_all();
                            return;
                        }

                        self.niri.layout.activate_window(&window);

                        if self
                            .niri
                            .layout
                            .interactive_resize_begin(window.clone(), edges)
                        {
                            let start_data = PointerGrabStartData {
                                focus: None,
                                button: button_code,
                                location,
                            };
                            let grab = ResizeGrab::new(start_data, window.clone());
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            self.niri
                                .cursor_manager
                                .set_cursor_image(CursorImageStatus::Named(edges.cursor_icon()));
                        }
                    }
                }

                if !is_overview_open {
                    self.niri.layout.activate_window(&window);
                }

                // FIXME: granular.
                self.niri.queue_redraw_all();
            } else if let Some((output, ws)) = is_overview_open
                .then(|| self.niri.workspace_under_cursor(false))
                .flatten()
            {
                let ws_idx = self.niri.layout.find_workspace_by_id(ws.id()).unwrap().0;

                self.niri.layout.focus_output(&output);
                self.niri.layout.toggle_overview_to_workspace(ws_idx);

                // FIXME: granular.
                self.niri.queue_redraw_all();
            } else if let Some(output) = self.niri.output_under_cursor() {
                self.niri.layout.focus_output(&output);

                // FIXME: granular.
                self.niri.queue_redraw_all();
            }
        };

        self.update_pointer_contents();

        if ButtonState::Pressed == button_state {
            let layer_under = self.niri.pointer_contents.layer.clone();
            self.niri.focus_layer_surface_if_on_demand(layer_under);
        }

        if button == Some(MouseButton::Left) && self.niri.screenshot_ui.is_open() {
            if button_state == ButtonState::Pressed {
                let pos = pointer.current_location();

                // If we'll be moving the existing selection, use the selection output.
                let output = if mod_down {
                    self.niri.screenshot_ui.selection_output()
                } else {
                    self.niri.output_under(pos).map(|(out, _)| out)
                };

                if let Some(output) = output.cloned() {
                    let geom = self.niri.global_space.output_geometry(&output).unwrap();
                    let point = (pos - geom.loc.to_f64())
                        .to_physical(output.current_scale().fractional_scale())
                        .to_i32_round();

                    if self
                        .niri
                        .screenshot_ui
                        .pointer_down(output, point, None, mod_down)
                    {
                        self.niri.queue_redraw_all();
                    }
                }
            } else if let Some(capture) = self.niri.screenshot_ui.pointer_up(None) {
                if capture {
                    self.confirm_screenshot(true);
                } else {
                    self.niri.queue_redraw_all();
                }
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                button: button_code,
                state: button_state,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        let pointer = &self.niri.seat.get_pointer().unwrap();

        let source = event.source();

        let mod_key = self.backend.mod_key(&self.niri.config.borrow());

        // We received an event for the regular pointer, so show it now. This is also needed for
        // update_pointer_contents() below to return the real contents, necessary for the pointer
        // axis event to reach the window.
        self.niri.pointer_visibility = PointerVisibility::Visible;

        let horizontal_amount_v120 = event.amount_v120(Axis::Horizontal);
        let vertical_amount_v120 = event.amount_v120(Axis::Vertical);

        let is_overview_open = self.niri.layout.is_overview_open();

        // We should only handle scrolling in the overview if the pointer is not over a (top or
        // overlay) layer surface.
        let should_handle_in_overview = if is_overview_open {
            // FIXME: ideally this should happen after updating the pointer contents, which happens
            // below. However, our pointer actions are supposed to act on the old surface, before
            // updating the pointer contents.
            pointer
                .current_focus()
                .map(|surface| self.niri.find_root_shell_surface(&surface))
                .is_none_or(|root| {
                    !self
                        .niri
                        .mapped_layer_surfaces
                        .keys()
                        .any(|layer| *layer.wl_surface() == root)
                })
        } else {
            false
        };

        // Handle wheel scroll bindings.
        if source == AxisSource::Wheel {
            // If we have a scroll bind with current modifiers, then accumulate and don't pass to
            // Wayland. If there's no bind, reset the accumulator.
            let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
            let modifiers = modifiers_from_state(mods);
            let should_handle =
                should_handle_in_overview || self.niri.mods_with_wheel_binds.contains(&modifiers);
            if should_handle {
                let horizontal = horizontal_amount_v120.unwrap_or(0.);
                let ticks = self.niri.horizontal_wheel_tracker.accumulate(horizontal);
                if ticks != 0 {
                    let (bind_left, bind_right) =
                        if should_handle_in_overview && modifiers.is_empty() {
                            let bind_left = Some(Bind {
                                key: Key {
                                    trigger: Trigger::WheelScrollLeft,
                                    modifiers: Modifiers::empty(),
                                },
                                action: Action::FocusColumnLeftUnderMouse,
                                repeat: true,
                                cooldown: None,
                                allow_when_locked: false,
                                hotkey_overlay_title: None,
                            });
                            let bind_right = Some(Bind {
                                key: Key {
                                    trigger: Trigger::WheelScrollRight,
                                    modifiers: Modifiers::empty(),
                                },
                                action: Action::FocusColumnRightUnderMouse,
                                repeat: true,
                                cooldown: None,
                                allow_when_locked: false,
                                hotkey_overlay_title: None,
                            });
                            (bind_left, bind_right)
                        } else {
                            let config = self.niri.config.borrow();
                            let bindings = make_binds_iter(&config);
                            let bind_left = find_configured_bind(
                                bindings.clone(),
                                mod_key,
                                Trigger::WheelScrollLeft,
                                mods,
                            )
                            .filter(|bind| {
                                !self.niri.screenshot_ui.is_open()
                                    || allowed_during_screenshot(&bind.action)
                            });
                            let bind_right = find_configured_bind(
                                bindings,
                                mod_key,
                                Trigger::WheelScrollRight,
                                mods,
                            )
                            .filter(|bind| {
                                !self.niri.screenshot_ui.is_open()
                                    || allowed_during_screenshot(&bind.action)
                            });
                            (bind_left, bind_right)
                        };

                    if let Some(right) = bind_right {
                        for _ in 0..ticks {
                            self.handle_bind(right.clone());
                        }
                    }
                    if let Some(left) = bind_left {
                        for _ in ticks..0 {
                            self.handle_bind(left.clone());
                        }
                    }
                }

                let vertical = vertical_amount_v120.unwrap_or(0.);
                let ticks = self.niri.vertical_wheel_tracker.accumulate(vertical);
                if ticks != 0 {
                    let (bind_up, bind_down) = if should_handle_in_overview && modifiers.is_empty()
                    {
                        let bind_up = Some(Bind {
                            key: Key {
                                trigger: Trigger::WheelScrollUp,
                                modifiers: Modifiers::empty(),
                            },
                            action: Action::FocusWorkspaceUpUnderMouse,
                            repeat: true,
                            cooldown: Some(Duration::from_millis(50)),
                            allow_when_locked: false,
                            hotkey_overlay_title: None,
                        });
                        let bind_down = Some(Bind {
                            key: Key {
                                trigger: Trigger::WheelScrollDown,
                                modifiers: Modifiers::empty(),
                            },
                            action: Action::FocusWorkspaceDownUnderMouse,
                            repeat: true,
                            cooldown: Some(Duration::from_millis(50)),
                            allow_when_locked: false,
                            hotkey_overlay_title: None,
                        });
                        (bind_up, bind_down)
                    } else if should_handle_in_overview && modifiers == Modifiers::SHIFT {
                        let bind_up = Some(Bind {
                            key: Key {
                                trigger: Trigger::WheelScrollUp,
                                modifiers: Modifiers::empty(),
                            },
                            action: Action::FocusColumnLeftUnderMouse,
                            repeat: true,
                            cooldown: Some(Duration::from_millis(50)),
                            allow_when_locked: false,
                            hotkey_overlay_title: None,
                        });
                        let bind_down = Some(Bind {
                            key: Key {
                                trigger: Trigger::WheelScrollDown,
                                modifiers: Modifiers::empty(),
                            },
                            action: Action::FocusColumnRightUnderMouse,
                            repeat: true,
                            cooldown: Some(Duration::from_millis(50)),
                            allow_when_locked: false,
                            hotkey_overlay_title: None,
                        });
                        (bind_up, bind_down)
                    } else {
                        let config = self.niri.config.borrow();
                        let bindings = make_binds_iter(&config);
                        let bind_up = find_configured_bind(
                            bindings.clone(),
                            mod_key,
                            Trigger::WheelScrollUp,
                            mods,
                        )
                        .filter(|bind| {
                            !self.niri.screenshot_ui.is_open()
                                || allowed_during_screenshot(&bind.action)
                        });
                        let bind_down =
                            find_configured_bind(bindings, mod_key, Trigger::WheelScrollDown, mods)
                                .filter(|bind| {
                                    !self.niri.screenshot_ui.is_open()
                                        || allowed_during_screenshot(&bind.action)
                                });
                        (bind_up, bind_down)
                    };

                    if let Some(down) = bind_down {
                        for _ in 0..ticks {
                            self.handle_bind(down.clone());
                        }
                    }
                    if let Some(up) = bind_up {
                        for _ in ticks..0 {
                            self.handle_bind(up.clone());
                        }
                    }
                }

                return;
            } else {
                self.niri.horizontal_wheel_tracker.reset();
                self.niri.vertical_wheel_tracker.reset();
            }
        }

        let horizontal_amount = event.amount(Axis::Horizontal);
        let vertical_amount = event.amount(Axis::Vertical);

        self.update_pointer_contents();

        let device_scroll_factor = {
            let config = self.niri.config.borrow();
            match source {
                AxisSource::Wheel => config.input.mouse.scroll_factor,
                _ => None,
            }
        };

        // Get window-specific scroll factor
        let window_scroll_factor = pointer
            .current_focus()
            .map(|focused| self.niri.find_root_shell_surface(&focused))
            .and_then(|root| self.niri.layout.find_window_and_output(&root).unzip().0)
            .and_then(|window| window.rules().scroll_factor)
            .unwrap_or(1.);

        // Determine final scroll factors based on configuration
        let (horizontal_factor, vertical_factor) = device_scroll_factor
            .map(|x| x.h_v_factors())
            .unwrap_or((1.0, 1.0));
        let (horizontal_factor, vertical_factor) = (
            horizontal_factor * window_scroll_factor,
            vertical_factor * window_scroll_factor,
        );

        let horizontal_amount = horizontal_amount.unwrap_or_else(|| {
            // Discrete scrolling.
            horizontal_amount_v120.unwrap_or(0.0) / 120. * 15.
        }) * horizontal_factor;

        let vertical_amount = vertical_amount.unwrap_or_else(|| {
            // Discrete scrolling.
            vertical_amount_v120.unwrap_or(0.0) / 120. * 15.
        }) * vertical_factor;

        let horizontal_amount_v120 = horizontal_amount_v120.map(|x| x * horizontal_factor);
        let vertical_amount_v120 = vertical_amount_v120.map(|x| x * vertical_factor);

        let mut frame = AxisFrame::new(event.time_msec()).source(source);
        if horizontal_amount != 0.0 {
            frame = frame
                .relative_direction(Axis::Horizontal, event.relative_direction(Axis::Horizontal));
            frame = frame.value(Axis::Horizontal, horizontal_amount);
            if let Some(v120) = horizontal_amount_v120 {
                frame = frame.v120(Axis::Horizontal, v120 as i32);
            }
        }
        if vertical_amount != 0.0 {
            frame =
                frame.relative_direction(Axis::Vertical, event.relative_direction(Axis::Vertical));
            frame = frame.value(Axis::Vertical, vertical_amount);
            if let Some(v120) = vertical_amount_v120 {
                frame = frame.v120(Axis::Vertical, v120 as i32);
            }
        }

        pointer.axis(self, frame);
        pointer.frame(self);
    }

    fn compute_absolute_location<I: InputBackend>(
        &self,
        evt: &impl AbsolutePositionEvent<I>,
        fallback_output: Option<&Output>,
    ) -> Option<Point<f64, Logical>> {
        let output = evt.device().output(self);
        let output = output.filter(|output| self.niri.output_exists(output));
        let output = output.as_ref().or(fallback_output)?;
        let output_geo = self.niri.global_space.output_geometry(output).unwrap();
        let transform = output.current_transform();
        let size = transform.invert().transform_size(output_geo.size);
        Some(
            transform.transform_point_in(evt.position_transformed(size), &size.to_f64())
                + output_geo.loc.to_f64(),
        )
    }

    pub fn is_dnd_grab(grab: &dyn Any) -> bool {
        // Normal DnD
        grab.is::<DnDGrab<Self, WlDataSource, WlSurface>>()
            // Null-source DnD: weston-dnd --self-only
            || grab.is::<DnDGrab<Self, WlSurface, WlSurface>>()
    }

    fn grab_can_be_cancelled_with_esc(grab: &(dyn PointerGrab<State> + 'static)) -> bool {
        let grab = grab.as_any();

        grab.is::<PickWindowGrab>() || grab.is::<PickColorGrab>() || Self::is_dnd_grab(grab)
    }
}

/// Check whether the key should be intercepted and mark intercepted
/// pressed keys as `suppressed`, thus preventing `releases` corresponding
/// to them from being delivered.
#[allow(clippy::too_many_arguments)]
fn should_intercept_key<'a>(
    suppressed_keys: &mut HashSet<Keycode>,
    bindings: impl IntoIterator<Item = &'a Bind>,
    mod_key: ModKey,
    key_code: Keycode,
    modified: Keysym,
    raw: Option<Keysym>,
    pressed: bool,
    mods: ModifiersState,
    screenshot_ui: &ScreenshotUi,
    disable_power_key_handling: bool,
) -> FilterResult<Option<Bind>> {
    // Actions are only triggered on presses, release of the key
    // shouldn't try to intercept anything unless we have marked
    // the key to suppress.
    if !pressed && !suppressed_keys.contains(&key_code) {
        return FilterResult::Forward;
    }

    let mut final_bind = find_bind(
        bindings,
        mod_key,
        modified,
        raw,
        mods,
        disable_power_key_handling,
    );

    // Allow only a subset of compositor actions while the screenshot UI is open, since the user
    // cannot see the screen.
    if screenshot_ui.is_open() {
        let mut use_screenshot_ui_action = true;

        if let Some(bind) = &final_bind
            && allowed_during_screenshot(&bind.action)
        {
            use_screenshot_ui_action = false;
        }

        if use_screenshot_ui_action && let Some(raw) = raw {
            final_bind = screenshot_ui.action(raw, mods).map(|action| Bind {
                key: Key {
                    trigger: Trigger::Keysym(raw),
                    // Not entirely correct but it doesn't matter in how we currently use it.
                    modifiers: Modifiers::empty(),
                },
                action,
                repeat: true,
                cooldown: None,
                allow_when_locked: false,
                // The screenshot UI owns the focus anyway, so this doesn't really matter.
                // But logically, nothing can inhibit its actions. Only opening it can be
                // inhibited.
                hotkey_overlay_title: None,
            });
        }
    }

    match (final_bind, pressed) {
        (Some(bind), true) => {
            suppressed_keys.insert(key_code);
            FilterResult::Intercept(Some(bind))
        }
        (_, false) => {
            // By this point, we know that the key was suppressed on press, so suppress the release.
            suppressed_keys.remove(&key_code);
            FilterResult::Intercept(None)
        }
        (None, true) => FilterResult::Forward,
    }
}

fn find_bind<'a>(
    bindings: impl IntoIterator<Item = &'a Bind>,
    mod_key: ModKey,
    modified: Keysym,
    raw: Option<Keysym>,
    mods: ModifiersState,
    disable_power_key_handling: bool,
) -> Option<Bind> {
    use keysyms::*;

    // Handle hardcoded binds.
    #[allow(non_upper_case_globals)] // wat
    let hardcoded_action = match modified.raw() {
        modified @ KEY_XF86Switch_VT_1..=KEY_XF86Switch_VT_12 => {
            let vt = (modified - KEY_XF86Switch_VT_1 + 1) as i32;
            Some(Action::ChangeVt(vt))
        }
        KEY_XF86PowerOff if !disable_power_key_handling => Some(Action::Suspend),
        _ => None,
    };

    if let Some(action) = hardcoded_action {
        return Some(Bind {
            key: Key {
                // Not entirely correct but it doesn't matter in how we currently use it.
                trigger: Trigger::Keysym(modified),
                modifiers: Modifiers::empty(),
            },
            action,
            repeat: true,
            cooldown: None,
            allow_when_locked: false,
            // In a worst-case scenario, the user has no way to unlock the compositor and a
            // misbehaving client has a keyboard shortcuts inhibitor, "jailing" the user.
            // The user must always be able to change VTs to recover from such a situation.
            // It also makes no sense to inhibit the default power key handling.
            // Hardcoded binds must never be inhibited.
            hotkey_overlay_title: None,
        });
    }

    let trigger = Trigger::Keysym(raw?);
    find_configured_bind(bindings, mod_key, trigger, mods)
}

fn find_configured_bind<'a>(
    bindings: impl IntoIterator<Item = &'a Bind>,
    mod_key: ModKey,
    trigger: Trigger,
    mods: ModifiersState,
) -> Option<Bind> {
    // Handle configured binds.
    let mut modifiers = modifiers_from_state(mods);

    let mod_down = modifiers_from_state(mods).contains(mod_key.to_modifiers());
    if mod_down {
        modifiers |= Modifiers::COMPOSITOR;
    }

    for bind in bindings {
        if bind.key.trigger != trigger {
            continue;
        }

        let mut bind_modifiers = bind.key.modifiers;
        if bind_modifiers.contains(Modifiers::COMPOSITOR) {
            bind_modifiers |= mod_key.to_modifiers();
        } else if bind_modifiers.contains(mod_key.to_modifiers()) {
            bind_modifiers |= Modifiers::COMPOSITOR;
        }

        if bind_modifiers == modifiers {
            return Some(bind.clone());
        }
    }

    None
}

fn modifiers_from_state(mods: ModifiersState) -> Modifiers {
    let mut modifiers = Modifiers::empty();
    if mods.ctrl {
        modifiers |= Modifiers::CTRL;
    }
    if mods.shift {
        modifiers |= Modifiers::SHIFT;
    }
    if mods.alt {
        modifiers |= Modifiers::ALT;
    }
    if mods.logo {
        modifiers |= Modifiers::SUPER;
    }
    if mods.iso_level3_shift {
        modifiers |= Modifiers::ISO_LEVEL3_SHIFT;
    }
    if mods.iso_level5_shift {
        modifiers |= Modifiers::ISO_LEVEL5_SHIFT;
    }
    modifiers
}

fn should_activate_monitors<I: InputBackend>(event: &InputEvent<I>) -> bool {
    match event {
        InputEvent::Keyboard { event } if event.state() == KeyState::Pressed => true,
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => true,
        InputEvent::PointerMotion { .. }
        | InputEvent::PointerMotionAbsolute { .. }
        | InputEvent::PointerAxis { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::GestureHoldBegin { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. } => true,
        // Ignore events like device additions and removals, key releases, gesture ends.
        _ => false,
    }
}

fn should_hide_hotkey_overlay<I: InputBackend>(event: &InputEvent<I>) -> bool {
    match event {
        InputEvent::Keyboard { event } if event.state() == KeyState::Pressed => true,
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => true,
        InputEvent::PointerAxis { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. } => true,
        _ => false,
    }
}

fn should_hide_exit_confirm_dialog<I: InputBackend>(event: &InputEvent<I>) -> bool {
    match event {
        InputEvent::Keyboard { event } if event.state() == KeyState::Pressed => true,
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => true,
        InputEvent::PointerAxis { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. } => true,
        _ => false,
    }
}

fn should_notify_activity<I: InputBackend>(event: &InputEvent<I>) -> bool {
    !matches!(
        event,
        InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
    )
}

fn should_reset_pointer_inactivity_timer<I: InputBackend>(event: &InputEvent<I>) -> bool {
    matches!(
        event,
        InputEvent::PointerAxis { .. }
            | InputEvent::PointerButton { .. }
            | InputEvent::PointerMotion { .. }
            | InputEvent::PointerMotionAbsolute { .. }
    )
}

fn allowed_when_locked(action: &Action) -> bool {
    matches!(
        action,
        Action::Quit(_)
            | Action::ChangeVt(_)
            | Action::Suspend
            | Action::PowerOffMonitors
            | Action::PowerOnMonitors
            | Action::SwitchLayout(_)
    )
}

fn allowed_during_screenshot(action: &Action) -> bool {
    matches!(
        action,
        Action::Quit(_)
            | Action::ChangeVt(_)
            | Action::Suspend
            | Action::PowerOffMonitors
            | Action::PowerOnMonitors
            // Intended for binds such as volume up/down, lock the screen, etc.
            | Action::Spawn(_)
            | Action::SpawnSh(_)
            // The screenshot UI can handle these.
            | Action::MoveColumnLeft
            | Action::MoveColumnLeftOrToMonitorLeft
            | Action::MoveColumnRight
            | Action::MoveColumnRightOrToMonitorRight
            | Action::MoveWindowUp
            | Action::MoveWindowUpOrToWorkspaceUp
            | Action::MoveWindowDown
            | Action::MoveWindowDownOrToWorkspaceDown
            | Action::MoveColumnToMonitorLeft
            | Action::MoveColumnToMonitorRight
            | Action::MoveColumnToMonitorUp
            | Action::MoveColumnToMonitorDown
            | Action::MoveColumnToMonitorPrevious
            | Action::MoveColumnToMonitorNext
            | Action::MoveColumnToMonitor(_)
            | Action::MoveWindowToMonitorLeft
            | Action::MoveWindowToMonitorRight
            | Action::MoveWindowToMonitorUp
            | Action::MoveWindowToMonitorDown
            | Action::MoveWindowToMonitorPrevious
            | Action::MoveWindowToMonitorNext
            | Action::MoveWindowToMonitor(_)
            | Action::SetWindowWidth(_)
            | Action::SetWindowHeight(_)
            | Action::SetColumnWidth(_)
    )
}

fn hardcoded_overview_bind(raw: Keysym, mods: ModifiersState) -> Option<Bind> {
    let mods = modifiers_from_state(mods);
    if !mods.is_empty() {
        return None;
    }

    let mut repeat = true;
    let action = match raw {
        Keysym::Escape | Keysym::Return => {
            repeat = false;
            Action::ToggleOverview
        }
        Keysym::Left => Action::FocusColumnLeft,
        Keysym::Right => Action::FocusColumnRight,
        Keysym::Up => Action::FocusWindowOrWorkspaceUp,
        Keysym::Down => Action::FocusWindowOrWorkspaceDown,
        _ => {
            return None;
        }
    };

    Some(Bind {
        key: Key {
            trigger: Trigger::Keysym(raw),
            modifiers: Modifiers::empty(),
        },
        action,
        repeat,
        cooldown: None,
        allow_when_locked: false,
        hotkey_overlay_title: None,
    })
}

pub fn apply_libinput_settings(config: &niri_config::Input, device: &mut input::Device) {
    // According to Mutter code, this setting is specific to touchpads. We don't configure
    // touchpads, but still use it to avoid applying mouse settings to one.
    let is_touchpad = device.config_tap_finger_count() > 0;

    let is_mouse = device.has_capability(input::DeviceCapability::Pointer) && !is_touchpad;
    if is_mouse {
        let c = &config.mouse;
        let _ = device.config_send_events_set_mode(if c.off {
            input::SendEventsMode::DISABLED
        } else {
            input::SendEventsMode::ENABLED
        });
        let _ = device.config_scroll_set_natural_scroll_enabled(c.natural_scroll);
        let _ = device.config_accel_set_speed(c.accel_speed.0);
        let _ = device.config_left_handed_set(c.left_handed);
        let _ = device.config_middle_emulation_set_enabled(c.middle_emulation);

        if let Some(accel_profile) = c.accel_profile {
            let _ = device.config_accel_set_profile(accel_profile.into());
        } else if let Some(default) = device.config_accel_default_profile() {
            let _ = device.config_accel_set_profile(default);
        }

        if let Some(method) = c.scroll_method {
            let _ = device.config_scroll_set_method(method.into());

            if method == niri_config::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        } else if let Some(default) = device.config_scroll_default_method() {
            let _ = device.config_scroll_set_method(default);

            if default == input::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        }
    }
}

pub fn mods_with_binds(mod_key: ModKey, binds: &Binds, triggers: &[Trigger]) -> HashSet<Modifiers> {
    let mut rv = HashSet::new();
    for bind in &binds.0 {
        if !triggers.contains(&bind.key.trigger) {
            continue;
        }

        let mut mods = bind.key.modifiers;
        if mods.contains(Modifiers::COMPOSITOR) {
            mods.remove(Modifiers::COMPOSITOR);
            mods.insert(mod_key.to_modifiers());
        }

        rv.insert(mods);
    }

    rv
}

pub fn mods_with_mouse_binds(mod_key: ModKey, binds: &Binds) -> HashSet<Modifiers> {
    mods_with_binds(
        mod_key,
        binds,
        &[
            Trigger::MouseLeft,
            Trigger::MouseRight,
            Trigger::MouseMiddle,
            Trigger::MouseBack,
            Trigger::MouseForward,
        ],
    )
}

pub fn mods_with_wheel_binds(mod_key: ModKey, binds: &Binds) -> HashSet<Modifiers> {
    mods_with_binds(
        mod_key,
        binds,
        &[
            Trigger::WheelScrollUp,
            Trigger::WheelScrollDown,
            Trigger::WheelScrollLeft,
            Trigger::WheelScrollRight,
        ],
    )
}

fn grab_allows_hot_corner(grab: &(dyn PointerGrab<State> + 'static)) -> bool {
    let grab = grab.as_any();

    // We lean on the blocklist approach here since it's not a terribly big deal if hot corner
    // works where it shouldn't, but it could prevent some workflows if the hot corner doesn't work
    // when it should.
    //
    // Some notable grabs not mentioned here:
    // - DnDGrab allows hot corner to DnD across workspaces.
    // - ClickGrab keeps pointer focus on the window, so the hot corner doesn't trigger.
    // - Touch grabs: touch doesn't trigger the hot corner.
    if grab.is::<ResizeGrab>() || grab.is::<SpatialMovementGrab>() {
        return false;
    }

    if let Some(grab) = grab.downcast_ref::<MoveGrab>() {
        // Window move allows hot corner to DnD across workspaces.
        if !grab.is_move() {
            return false;
        }
    }

    true
}

/// Returns an iterator over bindings.
fn make_binds_iter(config: &Config) -> impl Iterator<Item = &Bind> + Clone {
    config.binds.0.iter()
}
