#[macro_use]
extern crate tracing;

pub mod animation;
pub mod backend;
pub mod cli;
pub mod cursor;
pub mod frame_clock;
pub mod handlers;
pub mod input;
pub mod ipc;
pub mod layer;
pub mod layout;
pub mod niri;
// This module casts between numeric types at the boundary with OpenGL ES and
// with on-screen pixel geometry. Each cast category here has a concrete bound
// that makes it safe, and turning any of them fallible would only add panics
// to the render hot path with no way to recover from a "failure" that cannot
// actually occur:
//   - cast_precision_loss (i32/usize -> f32): values being converted are
//     screen-space pixel coordinates/sizes, which top out at real display
//     resolutions (order of 10^4 px). f32's 23-bit mantissa represents all
//     integers up to 2^24 (~16.7M) exactly, so no precision is lost.
//   - cast_possible_truncation (usize/f64 -> u32/i32/f32): the usize values
//     are buffer/vertex/damage-rect counts and texture unit indices, which
//     are always tiny (single- or double-digit) counts, never near u32::MAX;
//     the f64 -> f32 cases are scale factors and already-pixel-rounded
//     coordinates re-narrowed for GPU upload.
//   - cast_sign_loss (i32 -> u32): values are GL attribute/uniform locations
//     returned by the driver, which are contractually non-negative (the
//     only negative value, -1, signals "not found" and is checked before
//     this cast is reached).
//   - cast_possible_wrap (u32 -> i32): values are GL enum constants (e.g.
//     `ffi::LINEAR`, `ffi::CLAMP_TO_BORDER`) required by FFI signatures that
//     take `GLint`; core GL enums are always well under `i32::MAX`, so the
//     cast never actually wraps.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
pub mod render_helpers;
pub mod rubber_band;
pub mod ui;
pub mod utils;
pub mod window;
