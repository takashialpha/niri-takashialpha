//! Opens the primary GPU's DRM/GBM device, mirroring the relevant slice of
//! `niri::backend::tty::Tty::new`/`device_added` (session + DRM + GBM only;
//! no libinput/input backend, this spike never touches input).

use anyhow::{Context, bail};
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmNode, NodeType};
use smithay::backend::session::Session;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::udev;
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;

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

pub fn open_primary_gpu() -> anyhow::Result<DrmGbmDevice> {
    let (mut session, _notifier) = LibSeatSession::new().context(
        "error creating a session; run this from a real TTY, not a nested/sandboxed shell",
    )?;
    let seat_name = session.seat();

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
