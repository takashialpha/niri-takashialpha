//! Bridges the raw Vulkan handles matched in `vulkan_device` into a real
//! `wgpu::Device`/`wgpu::Queue`, reusing Smithay's already-created
//! `ash::Instance` rather than standing up a second `VkInstance`, and letting
//! wgpu-hal drive its own device creation (queue family selection, feature
//! negotiation) via `open_with_callback` rather than hand-rolling a raw
//! `ash::Device` ourselves. We only need to inject the three extra dmabuf
//! extensions into the extension list it builds.

use anyhow::Context;
use wgpu::hal::vulkan as hal_vulkan;

use crate::vulkan_device::{MatchedDevice, REQUIRED_DEVICE_EXTENSIONS};

pub struct Bridged {
    /// Held only to keep the wrapped Vulkan instance alive for as long as
    /// `device`/`queue` are in use. Never read directly.
    #[allow(dead_code)]
    pub instance: wgpu::Instance,
    /// Held only to keep the wrapped Vulkan adapter alive for as long as
    /// `device`/`queue` are in use. Never read directly.
    #[allow(dead_code)]
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

pub fn bridge_to_wgpu(matched: &MatchedDevice) -> anyhow::Result<Bridged> {
    // SAFETY: `raw_instance` is a live Vulkan instance created by
    // `smithay::backend::vulkan::Instance::with_extensions` using this same
    // `entry`, `instance_api_version`, and `extensions` list (see
    // `vulkan_device::match_physical_device`), matching `from_raw`'s
    // documented precondition instead of standing up a second, separate
    // `VkInstance`. The trailing `Some(no-op callback)` matters: passing
    // `None` there makes wgpu-hal's own `InstanceShared::drop` call
    // `vkDestroyInstance` on this handle too, double-destroying the same
    // `VkInstance` that `matched.smithay_instance` (kept alive for the
    // whole process, dropped after everything built from `hal_instance`,
    // see `main`'s local ordering) also owns and destroys. A no-op
    // callback here makes wgpu-hal skip its own destroy, leaving
    // `smithay_instance` as the sole real owner.
    let hal_instance = unsafe {
        hal_vulkan::Instance::from_raw(
            matched.entry.clone(),
            matched.smithay_instance.handle().clone(),
            matched.instance_api_version,
            0, // android_sdk_version: n/a
            None,
            matched.instance_extensions.clone(),
            matched.instance_flags,
            wgpu::MemoryBudgetThresholds::default(),
            false, // has_nv_optimus
            Some(Box::new(|| {})),
        )
    }
    .context("error bridging Smithay's Vulkan instance into wgpu-hal")?;

    let exposed_adapter = hal_instance
        .expose_adapter(matched.physical_device.handle())
        .context("wgpu-hal rejected the matched physical device as an adapter")?;

    let features = exposed_adapter.features;
    let limits = wgpu::Limits::default();
    let memory_hints = wgpu::MemoryHints::default();

    // SAFETY: the callback only ever adds to `extensions` (never removes),
    // as required by `open_with_callback`'s contract; the three dmabuf
    // extensions were already confirmed present on this physical device in
    // `vulkan_device::match_physical_device`.
    let open_device = unsafe {
        exposed_adapter.adapter.open_with_callback(
            features,
            &limits,
            &memory_hints,
            Some(Box::new(|args| {
                args.extensions
                    .extend_from_slice(REQUIRED_DEVICE_EXTENSIONS);
            })),
        )
    }
    .context("error opening the Vulkan device via wgpu-hal")?;

    // SAFETY: `hal_instance` is the same instance `expose_adapter` was called
    // on above, satisfying `from_hal`'s precondition.
    let instance = unsafe { wgpu::Instance::from_hal::<hal_vulkan::Api>(hal_instance) };
    // SAFETY: `exposed_adapter` was produced by `hal_instance.expose_adapter`
    // immediately above, from this same `instance`'s internal handle.
    let adapter = unsafe { instance.create_adapter_from_hal(exposed_adapter) };

    let device_descriptor = wgpu::DeviceDescriptor {
        label: Some("vulkan-spike"),
        required_features: features,
        required_limits: limits,
        memory_hints,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    };

    // SAFETY: `open_device` was produced by `adapter`'s underlying wgpu-hal
    // `Adapter` (via `exposed_adapter.adapter`) immediately above.
    let (device, queue) =
        unsafe { adapter.create_device_from_hal(open_device, &device_descriptor) }
            .context("error wrapping the opened Vulkan device into wgpu::Device")?;

    Ok(Bridged {
        instance,
        adapter,
        device,
        queue,
    })
}
