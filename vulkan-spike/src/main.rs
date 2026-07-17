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

use anyhow::Context;
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDeviceFd, DrmEvent};
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::output::{Mode as OutputMode, Output, OutputModeSource, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::ModeTypeFlags;
use smithay::reexports::gbm::Modifier;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner, SimpleCrtcMapper};

use renderer::WgpuRenderer;

/// Solid magenta: nothing else in a typical desktop looks like this, so it's
/// easy to eyeball whether the clear actually reached the screen.
const TEST_CLEAR_COLOR: [f32; 4] = [1.0, 0.0, 1.0, 1.0];

/// Exit after this many vblank-driven frames rather than running forever.
const FRAME_COUNT: u32 = 300;

type Compositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

// Flat sequential "open the device, then match Vulkan, then bridge wgpu,
// then set up KMS" bootstrap with no branching; it's long only because each
// step is spelled out, not because it's complex.
#[allow(clippy::too_many_lines)]
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let drm_gbm = device::open_primary_gpu()?;
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

    let mut scanner = DrmScanner::<SimpleCrtcMapper>::default();
    let (connector, crtc) = scanner
        .scan_connectors(&drm_device)
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

    let mut compositor: Compositor = DrmCompositor::new(
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
    .context("error creating the DRM compositor")?;

    let mut renderer = WgpuRenderer::new(bridged.device, bridged.queue);

    // First frame, kicked off synchronously so we don't need a calloop
    // "start" event to get going.
    render_and_queue(&mut compositor, &mut renderer)?;

    let mut event_loop: EventLoop<'_, ()> = EventLoop::try_new()?;
    let loop_signal = event_loop.get_signal();
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
