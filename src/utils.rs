use std::cmp::{max, min};
use std::ffi::{CString, OsStr};
use std::fmt::Display;
use std::io::Write;
use std::os::unix::prelude::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::Duration;
use std::{f64, fmt};

use anyhow::{Context, ensure};
use bitflags::bitflags;
use git_version::git_version;
use niri_config::{Config, OutputName};
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::input::pointer::CursorIcon;
use smithay::output::{self, Output};
use smithay::reexports::rustix::time::{ClockId, clock_gettime};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource as _};
use smithay::utils::{Coordinate, Logical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{SurfaceData, send_surface_state, with_states};
use smithay::wayland::fractional_scale::with_fractional_scale;
use smithay::wayland::shell::xdg::{
    ToplevelCachedState, ToplevelConfigure, ToplevelState, ToplevelSurface, XdgToplevelSurfaceData,
    XdgToplevelSurfaceRoleAttributes,
};
use wayland_backend::server::Credentials;

use crate::handlers::KdeDecorationsModeState;
use crate::niri::ClientState;

pub mod id;
pub mod scale;
pub mod signals;
pub mod spawning;
pub mod transaction;
pub mod vblank_throttle;
pub mod watcher;

use id::IdCounter;

/// Unique ID for a screencast session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CastSessionId(u64);

impl CastSessionId {
    pub fn next() -> Self {
        static COUNTER: IdCounter = IdCounter::new();
        Self(COUNTER.next())
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Display for CastSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique ID for a screencast stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CastStreamId(u64);

impl CastStreamId {
    pub fn next() -> Self {
        static COUNTER: IdCounter = IdCounter::new();
        Self(COUNTER.next())
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Display for CastStreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP          = 0b0001;
        const BOTTOM       = 0b0010;
        const LEFT         = 0b0100;
        const RIGHT        = 0b1000;

        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();

        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();

        const LEFT_RIGHT   = Self::LEFT.bits() | Self::RIGHT.bits();
        const TOP_BOTTOM   = Self::TOP.bits() | Self::BOTTOM.bits();
    }
}

// Infallible in practice: every discriminant of the generated `xdg_toplevel::ResizeEdge`
// enum (None, Top, Bottom, Left, Right, and the four corner combinations) is one of this
// bitflags type's known values, so `from_bits` can never return `None` here.
#[allow(clippy::fallible_impl_from)]
impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap()
    }
}

impl ResizeEdge {
    #[must_use]
    pub const fn cursor_icon(self) -> CursorIcon {
        match self {
            Self::LEFT => CursorIcon::WResize,
            Self::RIGHT => CursorIcon::EResize,
            Self::TOP => CursorIcon::NResize,
            Self::BOTTOM => CursorIcon::SResize,
            Self::TOP_LEFT => CursorIcon::NwResize,
            Self::TOP_RIGHT => CursorIcon::NeResize,
            Self::BOTTOM_RIGHT => CursorIcon::SeResize,
            Self::BOTTOM_LEFT => CursorIcon::SwResize,
            _ => CursorIcon::Default,
        }
    }
}

/// Build identifier: just the commit hash. There is no semantic version.
#[must_use]
pub fn version() -> String {
    option_env!("NIRI_BUILD_COMMIT")
        .unwrap_or(git_version!(fallback = "unknown commit"))
        .to_string()
}

// CLOCK_MONOTONIC always yields tv_sec >= 0 and 0 <= tv_nsec < 1_000_000_000 per POSIX, so
// these casts cannot lose sign or truncate.
#[must_use]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
pub fn get_monotonic_time() -> Duration {
    let ts = clock_gettime(ClockId::Monotonic);
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[must_use]
pub fn center(rect: Rectangle<i32, Logical>) -> Point<i32, Logical> {
    rect.loc + rect.size.downscale(2).to_point()
}

#[must_use]
pub fn center_f64(rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    rect.loc + rect.size.downscale(2.0).to_point()
}

/// Convert logical pixels to physical, rounding to physical pixels.
pub fn to_physical_precise_round<N: Coordinate>(scale: f64, logical: impl Coordinate) -> N {
    N::from_f64((logical.to_f64() * scale).round())
}

#[must_use]
pub fn round_logical_in_physical(scale: f64, logical: f64) -> f64 {
    (logical * scale).round() / scale
}

#[must_use]
pub fn round_logical_in_physical_max1(scale: f64, logical: f64) -> f64 {
    if logical == 0. {
        return 0.;
    }

    (logical * scale).max(1.).round() / scale
}

#[must_use]
pub fn floor_logical_in_physical_max1(scale: f64, logical: f64) -> f64 {
    if logical == 0. {
        return 0.;
    }

    (logical * scale).max(1.).floor() / scale
}

/// # Panics
///
/// Panics if `output` has no current mode set, which should not happen for any output
/// managed by niri.
#[must_use]
pub fn output_size(output: &Output) -> Size<f64, Logical> {
    let output_scale = output.current_scale().fractional_scale();
    let output_transform = output.current_transform();
    let output_mode = output.current_mode().unwrap();
    let logical_size = output_mode.size.to_f64().to_logical(output_scale);
    output_transform.transform_size(logical_size)
}

#[must_use]
pub fn logical_output(output: &Output) -> niri_ipc::LogicalOutput {
    let loc = output.current_location();
    let size = output_size(output);
    let transform = match output.current_transform() {
        Transform::Normal => niri_ipc::Transform::Normal,
        Transform::_90 => niri_ipc::Transform::_90,
        Transform::_180 => niri_ipc::Transform::_180,
        Transform::_270 => niri_ipc::Transform::_270,
        Transform::Flipped => niri_ipc::Transform::Flipped,
        Transform::Flipped90 => niri_ipc::Transform::Flipped90,
        Transform::Flipped180 => niri_ipc::Transform::Flipped180,
        Transform::Flipped270 => niri_ipc::Transform::Flipped270,
    };
    // Output dimensions are always positive and far below u32::MAX for any real display.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    niri_ipc::LogicalOutput {
        x: loc.x,
        y: loc.y,
        width: size.w as u32,
        height: size.h as u32,
        scale: output.current_scale().fractional_scale(),
        transform,
    }
}

pub struct PanelOrientation(pub Transform);
#[must_use]
pub fn panel_orientation(output: &Output) -> Transform {
    output
        .user_data()
        .get::<PanelOrientation>()
        .map_or(Transform::Normal, |x| x.0)
}

#[must_use]
pub const fn ipc_transform_to_smithay(transform: niri_ipc::Transform) -> Transform {
    match transform {
        niri_ipc::Transform::Normal => Transform::Normal,
        niri_ipc::Transform::_90 => Transform::_90,
        niri_ipc::Transform::_180 => Transform::_180,
        niri_ipc::Transform::_270 => Transform::_270,
        niri_ipc::Transform::Flipped => Transform::Flipped,
        niri_ipc::Transform::Flipped90 => Transform::Flipped90,
        niri_ipc::Transform::Flipped180 => Transform::Flipped180,
        niri_ipc::Transform::Flipped270 => Transform::Flipped270,
    }
}

#[must_use]
pub fn is_mapped(surface: &WlSurface) -> bool {
    // None if the surface hadn't committed yet.
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

pub fn send_scale_transform(
    surface: &WlSurface,
    data: &SurfaceData,
    scale: output::Scale,
    transform: Transform,
) {
    send_surface_state(surface, data, scale.integer_scale(), transform);
    with_fractional_scale(data, |fractional| {
        fractional.set_preferred_scale(scale.fractional_scale());
    });
}

/// # Errors
///
/// Returns an error if `path` starts with `~` but the home directory cannot be determined.
pub fn expand_home(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    if let Ok(rest) = path.strip_prefix("~") {
        let home = std::env::home_dir().context("error retrieving home directory")?;
        Ok(Some([home.as_path(), rest].iter().collect()))
    } else {
        Ok(None)
    }
}

/// # Errors
///
/// Returns an error if the configured path contains a NUL byte, if the current time cannot
/// be read or formatted, or if expanding a leading `~` fails.
pub fn make_screenshot_path(config: &Config) -> anyhow::Result<Option<PathBuf>> {
    let Some(path) = &config.screenshot_path.0 else {
        return Ok(None);
    };

    let format = CString::new(path.clone()).context("path must not contain nul bytes")?;

    let mut buf = [0u8; 2048];
    let mut path;
    unsafe {
        let time = libc::time(null_mut());
        ensure!(time != -1, "error in time()");

        let tm = libc::localtime(&raw const time);
        ensure!(!tm.is_null(), "error in localtime()");

        let rv = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr(), tm);
        ensure!(rv != 0, "error formatting time");

        path = PathBuf::from(OsStr::from_bytes(&buf[..rv]));
    }

    if let Some(expanded) = expand_home(&path).context("error expanding ~")? {
        path = expanded;
    }

    Ok(Some(path))
}

/// # Errors
///
/// Returns an error if writing the PNG header or image data fails.
pub fn write_png_rgba8(
    w: impl Write,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);

    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)
}

/// # Panics
///
/// Panics if `output` has no [`OutputName`] set, which should not happen for any output
/// managed by niri.
#[must_use]
pub fn output_matches_name(output: &Output, target: &str) -> bool {
    let name = output.user_data().get::<OutputName>().unwrap();
    name.matches(target)
}

/// # Panics
///
/// Panics if `toplevel`'s surface has no [`XdgToplevelSurfaceData`], which should not
/// happen for any surface with the xdg-toplevel role.
pub fn with_toplevel_role<T>(
    toplevel: &ToplevelSurface,
    f: impl FnOnce(&mut XdgToplevelSurfaceRoleAttributes) -> T,
) -> T {
    with_states(toplevel.wl_surface(), |states| {
        let mut role = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap();

        f(&mut role)
    })
}

/// # Panics
///
/// Panics if `toplevel`'s surface has no [`XdgToplevelSurfaceData`], which should not
/// happen for any surface with the xdg-toplevel role.
pub fn with_toplevel_role_and_current<T>(
    toplevel: &ToplevelSurface,
    f: impl FnOnce(&mut XdgToplevelSurfaceRoleAttributes, Option<&ToplevelState>) -> T,
) -> T {
    with_states(toplevel.wl_surface(), |states| {
        let mut role = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap();

        // `current` borrows out of `guard`, and `f` needs it live, so `guard` cannot be
        // dropped before the call to `f` below.
        #[allow(clippy::significant_drop_tightening)]
        let mut guard = states.cached_state.get::<ToplevelCachedState>();
        let current = guard.current().last_acked.as_ref().map(|c| &c.state);

        f(&mut role, current)
    })
}

/// # Panics
///
/// Panics if `toplevel`'s surface has no [`XdgToplevelSurfaceData`], which should not
/// happen for any surface with the xdg-toplevel role.
pub fn with_toplevel_last_uncommitted_configure<T>(
    toplevel: &ToplevelSurface,
    f: impl FnOnce(Option<&ToplevelConfigure>) -> T,
) -> T {
    with_states(toplevel.wl_surface(), |states| {
        let role = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap();

        let mut guard = states.cached_state.get::<ToplevelCachedState>();

        if let Some(last_pending) = role.pending_configures().last() {
            // Configure not yet acked by the client.
            drop(guard);
            f(Some(last_pending))
        } else if let Some(last_acked) = &role.last_acked {
            let already_committed = guard
                .current()
                .last_acked
                .as_ref()
                .is_some_and(|committed| committed.serial.is_no_older_than(&last_acked.serial));
            drop(guard);

            // Already committed to this configure.
            let configure = if already_committed { None } else { Some(last_acked) };

            f(configure)
        } else {
            // Surface hadn't been configured yet.
            drop(guard);
            f(None)
        }
    })
}

pub fn update_tiled_state(
    toplevel: &ToplevelSurface,
    prefer_no_csd: bool,
    force_tiled: Option<bool>,
) {
    // Determine the default value for our tiled state. The idea is to use the tiled state to
    // make windows rectangular even if they don't support xdg-decoration (e.g. GTK).
    //
    // If the user prefers no CSD, it's a reasonable assumption that they would prefer to get
    // rid of the various client-side rounded corners also by using the tiled state.
    // The map_or_else rewrite clippy suggests nests this whole three-way, heavily-commented
    // branch inside a closure, which reads worse than the plain if/else-if/else chain.
    #[allow(clippy::option_if_let_else)]
    let should_tile = || {
        // Figure out if the client bound any decoration globals for this window. In this case,
        // the pending decoration mode will be set to something (we always set it upon binding the
        // global and never reset to None).
        //
        // If the client bound a decoration global, use the mode that we negotiated. This way,
        // changing the decoration mode on the client at runtime will synchronize with the
        // default tiled state.
        if let Some(mode) = toplevel.with_pending_state(|state| state.decoration_mode) {
            mode == zxdg_toplevel_decoration_v1::Mode::ServerSide
        } else if let Some(mode) = with_states(toplevel.wl_surface(), |states| {
            states.data_map.get::<KdeDecorationsModeState>().cloned()
        }) {
            // Actually, make the KDE decoration overridable with prefer_no_csd. GTK 3 likes to
            // always request CSD through it, and we want prefer_no_csd to set the tiled state
            // automatically for GTK 3. Also, unlike xdg-decoration, KDE decoration is not
            // synchronized to commits, so that argument is less important.
            mode.is_server() || prefer_no_csd
        } else {
            // The client doesn't see or doesn't care about the decoration protocols. In this
            // case, use the current prefer_no_csd value as the user's intention.
            //
            // This is a bit weird because it makes it seem like prefer_no_csd can apply live,
            // while that isn't really the case. That's because prefer_no_csd controls two separate
            // things: whether the client sees the decoration globals, and the tiled state.
            //
            // A more accurate way would perhaps be to check if the client cannot see the
            // decoration globals, and in this case behave as if prefer_no_csd was false. However,
            // this also regresses the common case of GTK 4 applications that do not react to
            // xdg-decoration in any way, and therefore the tiled state *is* the "no CSD" mode from
            // the user's perspective, so by artificially gating it we would artificially make it
            // impossible to apply it live for GTK 4 applications.
            prefer_no_csd
        }
    };

    let should_tile = force_tiled.unwrap_or_else(should_tile);

    toplevel.with_pending_state(|state| {
        if should_tile {
            state.states.set(xdg_toplevel::State::TiledLeft);
            state.states.set(xdg_toplevel::State::TiledRight);
            state.states.set(xdg_toplevel::State::TiledTop);
            state.states.set(xdg_toplevel::State::TiledBottom);
        } else {
            state.states.unset(xdg_toplevel::State::TiledLeft);
            state.states.unset(xdg_toplevel::State::TiledRight);
            state.states.unset(xdg_toplevel::State::TiledTop);
            state.states.unset(xdg_toplevel::State::TiledBottom);
        }
    });
}

#[must_use]
pub fn get_credentials_for_surface(surface: &WlSurface) -> Option<Credentials> {
    let handle = surface.handle().upgrade()?;
    let dh = DisplayHandle::from(handle);

    let client = dh.get_client(surface.id()).ok()?;
    get_credentials_for_client(&dh, &client)
}

#[must_use]
/// # Panics
///
/// Panics if `client` has no [`ClientState`] data, which should not happen for any client
/// connected through niri's display handle.
pub fn get_credentials_for_client(dh: &DisplayHandle, client: &Client) -> Option<Credentials> {
    let data = client.get_data::<ClientState>().unwrap();
    if data.credentials_unknown {
        return None;
    }

    client.get_credentials(dh).ok()
}

#[must_use]
pub fn ensure_min_max_size(mut x: i32, min_size: i32, max_size: i32) -> i32 {
    if max_size > 0 {
        x = min(x, max_size);
    }
    if min_size > 0 {
        x = max(x, min_size);
    }
    x
}

#[must_use]
pub fn ensure_min_max_size_maybe_zero(x: i32, min_size: i32, max_size: i32) -> i32 {
    if x != 0 {
        ensure_min_max_size(x, min_size, max_size)
    } else if min_size > 0 && min_size == max_size {
        min_size
    } else {
        0
    }
}

pub fn clamp_preferring_top_left_in_area(
    area: Rectangle<f64, Logical>,
    rect: &mut Rectangle<f64, Logical>,
) {
    rect.loc.x = f64::min(rect.loc.x, area.loc.x + area.size.w - rect.size.w);
    rect.loc.y = f64::min(rect.loc.y, area.loc.y + area.size.h - rect.size.h);

    // Clamp by top and left last so it takes precedence.
    rect.loc.x = f64::max(rect.loc.x, area.loc.x);
    rect.loc.y = f64::max(rect.loc.y, area.loc.y);
}

#[must_use]
pub fn center_preferring_top_left_in_area(
    area: Rectangle<f64, Logical>,
    size: Size<f64, Logical>,
) -> Point<f64, Logical> {
    let area_size = area.size.to_point();
    let size = size.to_point();
    let mut offset = (area_size - size).downscale(2.);
    offset.x = f64::max(offset.x, 0.);
    offset.y = f64::max(offset.y, 0.);
    area.loc + offset
}

#[must_use]
pub fn baba_is_float_offset(now: Duration, view_height: f64) -> f64 {
    let now = now.as_secs_f64();
    let amplitude = view_height / 96.;
    amplitude * ((f64::consts::TAU * now / 3.6).sin() - 1.)
}
