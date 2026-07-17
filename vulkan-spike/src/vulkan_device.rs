//! Matches a Vulkan physical device to a DRM render node using Smithay's
//! `backend_vulkan` module (leans entirely on it: instance creation, physical
//! device enumeration, DRM-node matching, and extension/format-modifier
//! capability checks are all Smithay's existing code, not reinvented here).

use anyhow::{Context, bail};
use smithay::backend::drm::DrmNode;
use smithay::backend::vulkan::version::Version;
use smithay::backend::vulkan::{Instance as SmithayInstance, PhysicalDevice};
use smithay::reexports::gbm::Modifier;
use wgpu::hal::vulkan as hal_vulkan;

/// The exact format+modifier this spike renders into (see
/// `renderer::fourcc_to_wgpu_format` and the forced-`Linear` render formats
/// in `main.rs`). Checked once, up front, against what the matched physical
/// device actually reports supporting for external-memory `COLOR_ATTACHMENT`
/// import, rather than discovering an incompatibility only via a GPU hang
/// during the first real render.
const TARGET_VK_FORMAT: ash::vk::Format = ash::vk::Format::B8G8R8A8_UNORM;

/// The three extensions required for dmabuf import; checked up front against
/// the matched physical device rather than discovered via a failed
/// `vkCreateDevice`.
pub const REQUIRED_DEVICE_EXTENSIONS: &[&std::ffi::CStr] = &[
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
];

pub struct MatchedDevice {
    pub entry: ash::Entry,
    pub smithay_instance: SmithayInstance,
    pub physical_device: PhysicalDevice,
    pub instance_extensions: Vec<&'static std::ffi::CStr>,
    pub instance_api_version: u32,
    pub instance_flags: wgpu::InstanceFlags,
}

pub fn match_physical_device(render_node: DrmNode) -> anyhow::Result<MatchedDevice> {
    // SAFETY: loading the system Vulkan loader has no preconditions; this is a
    // fresh, independent `Entry` (just loaded function pointers), not tied to
    // whatever loader instance Smithay's `Instance` uses internally.
    let entry = unsafe { ash::Entry::load() }.context("error loading the Vulkan loader")?;

    // Ask wgpu-hal what instance extensions *it* wants, so Smithay's instance
    // gets created as a superset from the start (required by
    // `hal_vulkan::Instance::from_raw`'s safety contract) instead of trying to
    // patch this up after the fact.
    let instance_flags = if cfg!(debug_assertions) {
        wgpu::InstanceFlags::VALIDATION | wgpu::InstanceFlags::GPU_BASED_VALIDATION
    } else {
        wgpu::InstanceFlags::empty()
    };
    let wgpu_wanted_extensions = hal_vulkan::Instance::desired_extensions(
        &entry,
        Version::VERSION_1_3.to_raw(),
        instance_flags,
    )
    .context("error computing wgpu-hal's desired Vulkan instance extensions")?;

    // SAFETY: `with_extensions`'s preconditions are satisfied here: we're
    // requesting a well-formed extension list (wgpu-hal's own desired set,
    // which itself resolves inter-extension dependencies) with no additional
    // requirements beyond the standard `vkCreateInstance` usage.
    let smithay_instance = unsafe {
        SmithayInstance::with_extensions(Version::VERSION_1_3, None, &wgpu_wanted_extensions)
    }
    .context("error creating the Vulkan instance")?;

    let physical_device = PhysicalDevice::enumerate(&smithay_instance)
        .context("error enumerating Vulkan physical devices")?
        .find(|phd| matches!(phd.render_node(), Ok(Some(node)) if node == render_node))
        .with_context(|| format!("no Vulkan physical device matches render node {render_node}"))?;

    for ext in REQUIRED_DEVICE_EXTENSIONS {
        if !physical_device.has_device_extension(ext) {
            bail!(
                "physical device {:?} is missing required extension {:?}",
                physical_device.name(),
                ext
            );
        }
    }

    check_color_attachment_support(&physical_device, TARGET_VK_FORMAT, Modifier::Linear.into())?;

    // This is exactly the extension list `smithay_instance` was created with
    // above, so it's also exactly what `hal_vulkan::Instance::from_raw` needs
    // to be told was enabled.
    Ok(MatchedDevice {
        entry,
        smithay_instance,
        physical_device,
        instance_extensions: wgpu_wanted_extensions,
        instance_api_version: Version::VERSION_1_3.to_raw(),
        instance_flags,
    })
}

/// Confirms the physical device reports `COLOR_ATTACHMENT` tiling support for
/// `format` under `modifier`, via `VK_EXT_image_drm_format_modifier`'s
/// per-modifier properties. A dmabuf import + `vkCreateImage` can succeed at
/// the API level even when the hardware doesn't actually support using the
/// result as a render target for this specific format/modifier pair. That
/// mismatch doesn't surface as a clean Vulkan validation error, it surfaces
/// as GPU-side corruption or a hang partway through real use. Checking this
/// up front turns an unverified assumption into a checked precondition.
fn check_color_attachment_support(
    physical_device: &PhysicalDevice,
    format: ash::vk::Format,
    modifier: u64,
) -> anyhow::Result<()> {
    let properties = physical_device
        .get_format_modifier_properties(format)
        .with_context(|| format!("error querying DRM format modifier properties for {format:?}"))?;

    let supported = properties.iter().any(|props| {
        props.drm_format_modifier == modifier
            && props
                .drm_format_modifier_tiling_features
                .contains(ash::vk::FormatFeatureFlags::COLOR_ATTACHMENT)
    });

    if !supported {
        bail!(
            "physical device {:?} does not report COLOR_ATTACHMENT tiling support for \
             format {format:?} with DRM modifier {modifier:#x}; reported modifiers: {properties:?}",
            physical_device.name(),
        );
    }

    Ok(())
}
