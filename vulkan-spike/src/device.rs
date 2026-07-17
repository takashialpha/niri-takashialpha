//! Opens the primary GPU's DRM/GBM device, mirroring the relevant slice of
//! `niri::backend::tty::Tty::new`/`device_added` (session + DRM + GBM only;
//! no libinput/input backend, this spike never touches input).

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmNode, NodeType};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev;
use smithay::reexports::calloop::{EventLoop, LoopSignal};
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;

/// How long to wait for the seat daemon's activation handshake before giving
/// up. `LibSeatSession::new()` only catches an already-available activation
/// synchronously "in some cases" (see its own doc comment); the rest of the
/// time it arrives asynchronously and needs the event loop pumped to be
/// observed at all.
const SESSION_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

pub struct DrmGbmDevice {
    /// Held only to keep the seat/DRM-master access alive for the process's
    /// lifetime (dropping it would revoke DRM access). Never read directly.
    #[allow(dead_code)]
    pub session: LibSeatSession,
    pub drm_device: DrmDevice,
    pub drm_notifier: DrmDeviceNotifier,
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub render_node: DrmNode,
}

/// Opens the primary GPU's DRM/GBM device. `event_loop` must be pumped by
/// this function before touching the device at all: `LibSeatSession`'s
/// activation acknowledgement from the seat daemon (which is what actually
/// grants DRM master, not just `session.open()` succeeding) arrives as an
/// event on the session notifier, not synchronously at `LibSeatSession::new()`
/// time. Skipping this wait means every DRM atomic commit on the opened
/// device fails with EACCES, regardless of whether any other session is
/// competing for the seat.
///
/// `loop_signal` stops the whole event loop the moment the seat daemon asks
/// this session to pause (e.g. on a VT switch request). This spike doesn't
/// support pause/resume, so the only correct reaction to losing the seat is
/// to stop touching the device and exit cleanly, letting the process's own
/// teardown release DRM master. Without this, a VT-switch request left this
/// process still issuing DRM commits every vblank on a device it no longer
/// held, which is a plausible reason the switch itself never completed.
pub fn open_primary_gpu(
    event_loop: &mut EventLoop<'static, ()>,
    loop_signal: LoopSignal,
) -> anyhow::Result<DrmGbmDevice> {
    let (mut session, notifier) = LibSeatSession::new().context(
        "error creating a session; run this from a real TTY, not a nested/sandboxed shell",
    )?;
    let seat_name = session.seat();

    // Ownership moves into the loop here; the notifier (and the strong
    // reference it holds to the session's internals) stays alive for the
    // loop's lifetime, which is the whole program's, so `session` stays
    // usable throughout.
    event_loop
        .handle()
        .insert_source(notifier, move |event, (), ()| {
            tracing::debug!("session event: {event:?}");
            if matches!(event, SessionEvent::PauseSession) {
                tracing::info!("session paused (seat requested it, e.g. a VT switch); exiting");
                loop_signal.stop();
            }
        })
        .map_err(|err| anyhow::anyhow!("error registering the session event source: {err}"))?;

    let deadline = Instant::now() + SESSION_ACTIVATION_TIMEOUT;
    while !session.is_active() {
        if Instant::now() >= deadline {
            bail!(
                "session did not become active within {SESSION_ACTIVATION_TIMEOUT:?}; \
                 is another compositor or greeter still holding the seat?"
            );
        }
        event_loop
            .dispatch(Some(Duration::from_millis(100)), &mut ())
            .context("error dispatching the event loop while waiting for session activation")?;
    }
    tracing::info!("session is active");

    let primary_gpu_path = udev::primary_gpu(&seat_name)
        .context("error getting the primary GPU")?
        .context("couldn't find a GPU")?;
    let node =
        DrmNode::from_path(&primary_gpu_path).context("error opening the primary GPU DRM node")?;

    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = session
        .open(&primary_gpu_path, open_flags)
        .context("error opening the DRM device")?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm_device, drm_notifier) =
        DrmDevice::new(drm_fd.clone(), false).context("error creating the DRM device")?;
    let gbm = GbmDevice::new(drm_fd).context("error creating the GBM device")?;

    let Some(Ok(render_node)) = node.node_with_type(NodeType::Render) else {
        bail!("no render node available for the primary GPU ({node}); this spike requires one");
    };

    Ok(DrmGbmDevice {
        session,
        drm_device,
        drm_notifier,
        gbm,
        render_node,
    })
}
