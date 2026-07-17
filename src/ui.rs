pub mod config_error_notification;
pub mod exit_confirm_dialog;
pub mod hotkey_overlay;
pub mod screen_transition;
// This module casts screen-space pixel coordinates and sizes (already rounded
// via `.round()`) to i32/f32; like `render_helpers`, these values top out at real
// display resolutions, well within range, so no precision is meaningfully lost.
#[allow(clippy::cast_possible_truncation)]
pub mod screenshot_ui;
