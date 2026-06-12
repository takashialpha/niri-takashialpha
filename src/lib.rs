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
pub mod protocols;
pub mod render_helpers;
pub mod rubber_band;
pub mod ui;
pub mod utils;
pub mod window;

#[cfg(test)]
mod tests;
