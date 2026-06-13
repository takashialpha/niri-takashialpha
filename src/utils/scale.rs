//! Default monitor scale calculation.
//!
//! This module follows logic and tests from Mutter:
//! <https://gitlab.gnome.org/GNOME/mutter/-/blob/gnome-46/src/backends/meta-monitor.c>

use smithay::utils::{Physical, Raw, Size};

const MIN_SCALE: i32 = 1;
const MAX_SCALE: i32 = 4;
const STEPS: i32 = 4;
const MIN_LOGICAL_AREA: i32 = 800 * 480;

const MOBILE_TARGET_DPI: f64 = 135.;
const LARGE_TARGET_DPI: f64 = 110.;
const LARGE_MIN_SIZE_INCHES: f64 = 20.;

/// Calculates the ideal scale for a monitor.
pub fn guess_monitor_scale(size_mm: Size<i32, Raw>, resolution: Size<i32, Physical>) -> f64 {
    if size_mm.w == 0 || size_mm.h == 0 {
        return 1.;
    }

    let diag_inches = f64::from(size_mm.w * size_mm.w + size_mm.h * size_mm.h).sqrt() / 25.4;

    let target_dpi = if diag_inches < LARGE_MIN_SIZE_INCHES {
        MOBILE_TARGET_DPI
    } else {
        LARGE_TARGET_DPI
    };

    let physical_dpi =
        f64::from(resolution.w * resolution.w + resolution.h * resolution.h).sqrt() / diag_inches;
    let perfect_scale = physical_dpi / target_dpi;

    supported_scales(resolution)
        .map(|scale| (scale, (scale - perfect_scale).abs()))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map_or(1., |(scale, _)| scale)
}

pub fn supported_scales(resolution: Size<i32, Physical>) -> impl Iterator<Item = f64> {
    (MIN_SCALE * STEPS..=MAX_SCALE * STEPS)
        .map(|x| f64::from(x) / f64::from(STEPS))
        .filter(move |scale| is_valid_for_resolution(resolution, *scale))
}

fn is_valid_for_resolution(resolution: Size<i32, Physical>, scale: f64) -> bool {
    let logical = resolution.to_f64().to_logical(scale).to_i32_round::<i32>();
    logical.w * logical.h >= MIN_LOGICAL_AREA
}

/// Adjusts the scale to the closest exactly-representable value.
pub fn closest_representable_scale(scale: f64) -> f64 {
    // Current fractional-scale Wayland protocol can only represent N / 120 scales.
    const FRACTIONAL_SCALE_DENOM: f64 = 120.;

    (scale * FRACTIONAL_SCALE_DENOM).round() / FRACTIONAL_SCALE_DENOM
}
