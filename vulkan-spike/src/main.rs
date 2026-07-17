//! Standalone spike: prove a wgpu-on-Vulkan renderer can drive Smithay's
//! `DrmCompositor` and get a solid-color-cleared frame scanned out via real
//! KMS. Isolated from the main `niri` crate on purpose, so it can fail,
//! get rewritten, or be deleted without touching anything niri actually runs.
//!
//! v1 scope: clear the whole output to a solid test color, nothing else. No
//! client buffers, no textures, no shaders, no `NiriRenderer`/`render_helpers`
//! involvement.

mod device;
mod renderer;
mod vulkan_device;
mod wgpu_bridge;

use std::time::Duration;

use anyhow::Context;
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEvent};
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::output::{Mode as OutputMode, Output, OutputModeSource, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::{EventLoop, LoopSignal};
use smithay::reexports::drm::control::ModeTypeFlags;
use smithay::reexports::gbm::Modifier;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner, SimpleCrtcMapper};

use renderer::WgpuRenderer;

/// Solid magenta: nothing else in a typical desktop looks like this, so it's
/// easy to eyeball whether the clear actually reached the screen.
const TEST_CLEAR_COLOR: [f32; 4] = [1.0, 0.0, 1.0, 1.0];

/// Exit after this many vblank-driven frames rather than running forever.
const FRAME_COUNT: u32 = 300;

/// Absolute hard deadline for the whole program, from process start.
/// Deliberately independent of the calloop event loop and everything it
/// drives: if a DRM ioctl, a GPU wait, or anything else blocks indefinitely
/// on the main thread for any reason, this is a separate OS thread and
/// cannot be blocked by that. It unconditionally kills the process instead
/// of leaving the session (and VT switching) stuck with no way out. The
/// whole 300-frame test normally completes in well under this.
const WATCHDOG_TIMEOUT: Duration = Duration::from_mins(1);

/// `std::process::exit` skips Rust destructors, but that's fine here: it
/// doesn't need to be a clean shutdown, it needs to be an unconditional one.
/// Closing this process's file descriptors (including DRM master) and
/// reclaiming its GPU-side resources on exit is the kernel's job, not
/// something that depends on this process's own cleanup code running (which
/// could itself be part of what's stuck). Must be called after
/// `tracing_subscriber::fmt::init()` so the watchdog's own message actually
/// gets printed.
fn spawn_watchdog() {
    let spawned = std::thread::Builder::new()
        .name("watchdog".to_owned())
        .spawn(|| {
            std::thread::sleep(WATCHDOG_TIMEOUT);
            tracing::error!(
                "watchdog fired, did not finish within {WATCHDOG_TIMEOUT:?}; forcing exit"
            );
            std::process::exit(1);
        });
    if let Err(err) = spawned {
        // Failing to spawn the safety net is itself worth failing loudly
        // over, rather than silently continuing without one.
        tracing::warn!("error spawning the watchdog thread: {err}");
    }
}

type Compositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    spawn_watchdog();

    // Created up front (rather than later, right before the vblank loop)
    // because `device::open_primary_gpu` itself needs to pump this loop to
    // observe the seat daemon's asynchronous session-activation event before
    // it's safe to touch the DRM device at all.
    let mut event_loop: EventLoop<'static, ()> = EventLoop::try_new()?;
    let loop_signal = event_loop.get_signal();

    let drm_gbm = device::open_primary_gpu(&mut event_loop, loop_signal.clone())?;
    tracing::info!("opened primary GPU, render node: {}", drm_gbm.render_node);

    let matched = vulkan_device::match_physical_device(drm_gbm.render_node)?;
    tracing::info!(
        "matched Vulkan physical device: {}",
        matched.physical_device.name()
    );

    let bridged = wgpu_bridge::bridge_to_wgpu(&matched)?;
    tracing::info!("bridged into wgpu");

    let device::DrmGbmDevice {
        mut drm_device,
        drm_notifier,
        gbm,
        ..
    } = drm_gbm;

    let compositor = open_compositor(&mut drm_device, gbm)?;
    let renderer = WgpuRenderer::new(bridged.device, bridged.queue);

    run(event_loop, loop_signal, drm_notifier, compositor, renderer)
}

/// Finds a connected display with a free CRTC and builds a `DrmCompositor`
/// for it, using its preferred mode (or its first mode, if none is marked
/// preferred).
fn open_compositor(
    drm_device: &mut DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
) -> anyhow::Result<Compositor> {
    let mut scanner = DrmScanner::<SimpleCrtcMapper>::default();
    let (connector, crtc) = scanner
        .scan_connectors(drm_device)
        .context("error scanning DRM connectors")?
        .into_iter()
        .find_map(|event| match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => Some((connector, crtc)),
            _ => None,
        })
        .context("no connected display with a free CRTC found")?;

    let mode = *connector
        .modes()
        .iter()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
        .context("connector has no modes")?;

    tracing::info!(
        "using connector {}-{}, mode {:?}",
        connector.interface().as_str(),
        connector.interface_id(),
        mode
    );

    let surface = drm_device
        .create_surface(crtc, mode, &[connector.handle()])
        .context("error creating the DRM surface")?;

    let output = Output::new(
        "vulkan-spike".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "vulkan-spike".to_string(),
            model: "vulkan-spike".to_string(),
            serial_number: "0".to_string(),
        },
    );
    let (width, height) = mode.size();
    let wl_mode = OutputMode {
        size: (i32::from(width), i32::from(height)).into(),
        refresh: mode.vrefresh().cast_signed() * 1000,
    };
    output.change_current_state(Some(wl_mode), None, None, None);
    output.set_preferred(wl_mode);

    let gbm_flags = GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT;
    let allocator = GbmAllocator::new(gbm.clone(), gbm_flags);
    let exporter = GbmFramebufferExporter::new(gbm.clone(), None.into());

    // Restrict to linear buffers for this first version. Simpler format,
    // most likely to be broadly accepted for external-memory/COLOR_ATTACHMENT
    // import.
    let render_formats: FormatSet = [Fourcc::Xrgb8888, Fourcc::Argb8888]
        .into_iter()
        .map(|code| smithay::backend::allocator::Format {
            code,
            modifier: Modifier::Linear,
        })
        .collect();

    DrmCompositor::new(
        OutputModeSource::Auto(output.downgrade()),
        surface,
        None,
        allocator,
        exporter,
        [Fourcc::Xrgb8888, Fourcc::Argb8888],
        render_formats,
        drm_device.cursor_size(),
        Some(gbm),
    )
    .context("error creating the DRM compositor")
}

/// Renders and presents the first frame synchronously (so no calloop "start"
/// event is needed to get going), then drives the rest from vblank events
/// until `FRAME_COUNT` is reached or an error stops the loop.
fn run(
    mut event_loop: EventLoop<'static, ()>,
    loop_signal: LoopSignal,
    drm_notifier: DrmDeviceNotifier,
    mut compositor: Compositor,
    mut renderer: WgpuRenderer,
) -> anyhow::Result<()> {
    render_and_queue(&mut compositor, &mut renderer)?;

    let mut frames_left = FRAME_COUNT;
    event_loop
        .handle()
        .insert_source(drm_notifier, move |event, _, ()| match event {
            DrmEvent::VBlank(_) => {
                if let Err(err) = compositor.frame_submitted() {
                    tracing::error!("error marking frame as submitted: {err:?}");
                    loop_signal.stop();
                    return;
                }

                frames_left -= 1;
                if frames_left == 0 {
                    tracing::info!("reached frame count, exiting");
                    loop_signal.stop();
                    return;
                }

                if let Err(err) = render_and_queue(&mut compositor, &mut renderer) {
                    tracing::error!("error rendering/queueing next frame: {err:?}");
                    loop_signal.stop();
                }
            }
            DrmEvent::Error(err) => {
                tracing::error!("DRM error event: {err:?}");
                loop_signal.stop();
            }
        })
        .map_err(|err| anyhow::anyhow!("error registering the DRM event source: {err}"))?;

    event_loop.run(None, &mut (), |()| {})?;

    Ok(())
}

fn render_and_queue(
    compositor: &mut Compositor,
    renderer: &mut WgpuRenderer,
) -> anyhow::Result<()> {
    // `SolidColorRenderElement` implements `RenderElement<R>` generically for
    // any `R: Renderer` (see smithay's `element::solid` module). Reused here
    // only to give the always-empty `elements` slice a concrete type; v1
    // never actually constructs one.
    let elements: &[SolidColorRenderElement] = &[];

    let result = compositor
        .render_frame(renderer, elements, TEST_CLEAR_COLOR, FrameFlags::DEFAULT)
        .map_err(|err| anyhow::anyhow!("error rendering frame: {err:?}"))?;

    if !result.is_empty {
        compositor
            .queue_frame(())
            .map_err(|err| anyhow::anyhow!("error queueing frame: {err:?}"))?;
    }

    Ok(())
}
