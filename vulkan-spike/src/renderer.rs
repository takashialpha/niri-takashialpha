//! Minimal Smithay `Renderer`/`Frame`/`Bind<Dmabuf>` implementation backed by
//! wgpu. Scoped to exactly what the v1 spike needs: clear a bound `Dmabuf`
//! target to a solid color. No textured rendering yet
//! (`render_texture_from_to` is `unimplemented!()`), no touching niri's real
//! `NiriRenderer`/`render_helpers`.
//!
//! The novel piece is `Bind<Dmabuf>::bind`: `DrmCompositor`'s `GbmAllocator`
//! already allocates the dmabuf, so this only ever needs to *import* it as a
//! wgpu render target, never export one.

use std::fmt;
use std::os::fd::OwnedFd;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer, Fourcc};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, DebugFlags, Frame as SmithayFrame, Renderer, RendererSuper,
    Texture as SmithayTexture, TextureFilter,
};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};
use wgpu::hal::vulkan as hal_vulkan;

#[derive(Debug)]
pub enum WgpuRendererError {
    Dmabuf(String),
}

impl fmt::Display for WgpuRendererError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dmabuf(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for WgpuRendererError {}

#[derive(Debug)]
pub struct WgpuTexture {
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

impl SmithayTexture for WgpuTexture {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn format(&self) -> Option<Fourcc> {
        None
    }
}

pub struct WgpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    context_id: ContextId<WgpuTexture>,
    debug_flags: DebugFlags,
}

impl fmt::Debug for WgpuRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WgpuRenderer").finish_non_exhaustive()
    }
}

impl WgpuRenderer {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self {
            device,
            queue,
            context_id: ContextId::new(),
            debug_flags: DebugFlags::empty(),
        }
    }
}

impl RendererSuper for WgpuRenderer {
    type Error = WgpuRendererError;
    type TextureId = WgpuTexture;
    type Framebuffer<'buffer> = WgpuTexture;
    type Frame<'frame, 'buffer>
        = WgpuFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
        Self: 'frame;
}

impl Renderer for WgpuRenderer {
    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.context_id.clone()
    }

    fn downscale_filter(&mut self, _filter: TextureFilter) -> Result<(), Self::Error> {
        Ok(())
    }

    fn upscale_filter(&mut self, _filter: TextureFilter) -> Result<(), Self::Error> {
        Ok(())
    }

    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.debug_flags = flags;
    }

    fn debug_flags(&self) -> DebugFlags {
        self.debug_flags
    }

    fn render<'frame, 'buffer>(
        &'frame mut self,
        framebuffer: &'frame mut Self::Framebuffer<'buffer>,
        output_size: Size<i32, Physical>,
        dst_transform: Transform,
    ) -> Result<Self::Frame<'frame, 'buffer>, Self::Error>
    where
        'buffer: 'frame,
    {
        let encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vulkan-spike frame"),
            });
        Ok(WgpuFrame {
            device: &self.device,
            queue: &self.queue,
            context_id: self.context_id.clone(),
            encoder: Some(encoder),
            target: &*framebuffer,
            output_size,
            transform: dst_transform,
            _buffer: std::marker::PhantomData,
        })
    }

    fn wait(&mut self, _sync: &SyncPoint) -> Result<(), Self::Error> {
        // `WgpuFrame::finish` blocks on `device.poll` until the submitted
        // work completes, so every `SyncPoint` we ever hand out is already
        // signaled by the time it's observed here.
        Ok(())
    }
}

impl Bind<Dmabuf> for WgpuRenderer {
    fn bind<'a>(&mut self, target: &'a mut Dmabuf) -> Result<Self::Framebuffer<'a>, Self::Error> {
        import_dmabuf_as_color_target(&self.device, target)
    }
}

pub struct WgpuFrame<'frame, 'buffer> {
    device: &'frame wgpu::Device,
    queue: &'frame wgpu::Queue,
    context_id: ContextId<WgpuTexture>,
    encoder: Option<wgpu::CommandEncoder>,
    target: &'frame WgpuTexture,
    output_size: Size<i32, Physical>,
    transform: Transform,
    // `WgpuTexture` owns everything it needs (no internal borrow), so
    // `'buffer` isn't structurally required here. But `RendererSuper`'s GAT
    // bound (`'buffer: 'frame`) needs a real type parameter to attach to, not
    // just an implicit elided one, or trait-impl lifetime elision becomes
    // ambiguous for callers (e.g. `RenderElement::draw`'s `R::Frame<'_, '_>`).
    _buffer: std::marker::PhantomData<&'buffer mut WgpuTexture>,
}

impl WgpuFrame<'_, '_> {
    fn clear_region(&mut self, color: Color32F) {
        let encoder = self
            .encoder
            .as_mut()
            .expect("clear/draw_solid called after finish()");
        let [r, g, b, a] = color.components();
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vulkan-spike clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: f64::from(r),
                        g: f64::from(g),
                        b: f64::from(b),
                        a: f64::from(a),
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        drop(pass);
    }
}

impl SmithayFrame for WgpuFrame<'_, '_> {
    type Error = WgpuRendererError;
    type TextureId = WgpuTexture;

    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.context_id.clone()
    }

    fn clear(
        &mut self,
        color: Color32F,
        _at: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        // v1 spike: `at` is ignored, the whole target always gets cleared.
        self.clear_region(color);
        Ok(())
    }

    fn draw_solid(
        &mut self,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _color: Color32F,
    ) -> Result<(), Self::Error> {
        unimplemented!("draw_solid is out of scope for the v1 clear-only spike")
    }

    fn render_texture_from_to(
        &mut self,
        _texture: &Self::TextureId,
        _src: Rectangle<f64, BufferCoord>,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _src_transform: Transform,
        _alpha: f32,
    ) -> Result<(), Self::Error> {
        unimplemented!("textured rendering is out of scope for the v1 clear-only spike")
    }

    fn transformation(&self) -> Transform {
        self.transform
    }

    fn output_size(&self) -> Size<i32, Physical> {
        self.output_size
    }

    fn wait(&mut self, _sync: &SyncPoint) -> Result<(), Self::Error> {
        Ok(())
    }

    fn finish(mut self) -> Result<SyncPoint, Self::Error> {
        let encoder = self.encoder.take().expect("encoder taken twice");
        self.queue.submit(Some(encoder.finish()));
        // Block until the GPU has actually finished, so the caller (the
        // `DrmCompositor` presentation loop) can safely treat the target as
        // ready to scan out the moment this returns.
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| WgpuRendererError::Dmabuf(format!("error polling device: {err}")))?;
        Ok(SyncPoint::signaled())
    }
}

const fn fourcc_to_wgpu_format(fourcc: Fourcc) -> Option<wgpu::TextureFormat> {
    match fourcc {
        // DRM's XRGB8888/ARGB8888 are little-endian byte order B,G,R,(X|A),
        // matching wgpu's Bgra8Unorm byte layout.
        Fourcc::Argb8888 | Fourcc::Xrgb8888 => Some(wgpu::TextureFormat::Bgra8Unorm),
        // XBGR8888/ABGR8888 are R,G,B,(X|A), matching Rgba8Unorm.
        Fourcc::Abgr8888 | Fourcc::Xbgr8888 => Some(wgpu::TextureFormat::Rgba8Unorm),
        _ => None,
    }
}

/// Imports a dmabuf (already allocated by `DrmCompositor`'s `GbmAllocator`)
/// as a wgpu render target. Only ever imports, never exports one, since the
/// allocator already did that part.
fn import_dmabuf_as_color_target(
    device: &wgpu::Device,
    dmabuf: &Dmabuf,
) -> Result<WgpuTexture, WgpuRendererError> {
    if dmabuf.num_planes() != 1 {
        return Err(WgpuRendererError::Dmabuf(format!(
            "only single-plane dmabufs are supported by this spike (got {})",
            dmabuf.num_planes()
        )));
    }

    let width = dmabuf.width();
    let height = dmabuf.height();
    let format = dmabuf.format();
    let stride = dmabuf
        .strides()
        .next()
        .ok_or_else(|| WgpuRendererError::Dmabuf("dmabuf has no strides".into()))?;
    let offset = dmabuf.offsets().next().unwrap_or(0);
    let modifier: u64 = format.modifier.into();

    let borrowed_fd = dmabuf
        .handles()
        .next()
        .ok_or_else(|| WgpuRendererError::Dmabuf("dmabuf has no fd".into()))?;
    let owned_fd: OwnedFd = borrowed_fd.try_clone_to_owned().map_err(|err| {
        WgpuRendererError::Dmabuf(format!("error duplicating the dmabuf fd: {err}"))
    })?;

    let wgpu_format = fourcc_to_wgpu_format(format.code).ok_or_else(|| {
        WgpuRendererError::Dmabuf(format!("unsupported dmabuf pixel format {:?}", format.code))
    })?;

    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };

    // The hal-level descriptor, for `texture_from_dmabuf_fd`. Note its
    // `usage` is `wgpu::TextureUses` (the internal hal usage-tracking bitset),
    // not the public `wgpu::TextureUsages`.
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("vulkan-spike dmabuf target"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu_format,
        usage: wgpu::TextureUses::COLOR_TARGET,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };

    // SAFETY: `device` was created (see `wgpu_bridge::bridge_to_wgpu`) with
    // the Vulkan backend and the three dmabuf-import extensions confirmed
    // present and enabled, so this `as_hal` call is guaranteed to succeed.
    let hal_device = unsafe { device.as_hal::<hal_vulkan::Api>() }.ok_or_else(|| {
        WgpuRendererError::Dmabuf("device is not backed by the Vulkan hal".into())
    })?;

    // SAFETY: `owned_fd` is a valid, just-duplicated dmabuf fd; `hal_desc`,
    // `modifier`, `stride`, and `offset` are all taken directly from this
    // same `dmabuf` (checked single-plane above), matching
    // `texture_from_dmabuf_fd`'s documented precondition that they describe
    // the same buffer layout.
    let hal_texture = unsafe {
        hal_device.texture_from_dmabuf_fd(
            owned_fd,
            &hal_desc,
            modifier,
            u64::from(stride),
            u64::from(offset),
        )
    }
    .map_err(|err| WgpuRendererError::Dmabuf(format!("error importing dmabuf: {err:?}")))?;
    drop(hal_device);

    // The public-API descriptor, for `create_texture_from_hal`. Same
    // logical texture as `hal_desc` above, but `create_texture_from_hal`
    // wants `wgpu::TextureDescriptor` (public `TextureUsages`), not the hal
    // one.
    let public_desc = wgpu::TextureDescriptor {
        label: Some("vulkan-spike dmabuf target"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    };

    // SAFETY: `hal_texture` was just imported above via
    // `texture_from_dmabuf_fd` using a descriptor describing the same
    // texture as `public_desc`. A freshly imported external-memory image
    // comes out of that call in `VK_IMAGE_LAYOUT_UNDEFINED`, so
    // `TextureUses::UNINITIALIZED` reflects that actual state rather than
    // assuming one, per `create_texture_from_hal`'s safety note.
    let texture = unsafe {
        device.create_texture_from_hal::<hal_vulkan::Api>(
            hal_texture,
            &public_desc,
            wgpu::TextureUses::UNINITIALIZED,
        )
    };

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    Ok(WgpuTexture {
        view,
        width,
        height,
    })
}
