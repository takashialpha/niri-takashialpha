use anyhow::Context as _;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Scale, Size};

use crate::animation::Animation;
use crate::niri_render_elements;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenData, OffscreenRenderElement};
use crate::render_helpers::shader_element::ShaderRenderElement;

#[derive(Debug)]
pub struct OpenAnimation {
    anim: Animation,
    buffer: OffscreenBuffer,
}

niri_render_elements! {
    OpeningWindowRenderElement => {
        Offscreen = RelocateRenderElement<RescaleRenderElement<OffscreenRenderElement>>,
        Shader = ShaderRenderElement,
    }
}

impl OpenAnimation {
    #[must_use]
    pub fn new(anim: Animation) -> Self {
        Self {
            anim,
            buffer: OffscreenBuffer::default(),
        }
    }

    pub fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    /// We can't depend on `view_rect` here, because the result of window opening can be snapshot and
    /// then rendered elsewhere.
    ///
    /// # Errors
    ///
    /// Returns an error if rendering `elements` to the offscreen buffer fails.
    // Logical-pixel geometry and texture dimensions are narrowed to f32 for the GL shader;
    // neither ever approaches f32's precision limits.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn render(
        &self,
        renderer: &mut GlesRenderer,
        elements: &[impl RenderElement<GlesRenderer>],
        geo_size: Size<f64, Logical>,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> anyhow::Result<(OpeningWindowRenderElement, OffscreenData)> {
        let progress = self.anim.value();
        let clamped_progress = self.anim.clamped_value().clamp(0., 1.);

        let (elem, _sync_point, data) = self
            .buffer
            .render(renderer, scale, elements)
            .context("error rendering to offscreen buffer")?;

        let elem = elem.with_alpha(clamped_progress as f32 * alpha);

        let center = geo_size.to_point().downscale(2.);
        let elem = RescaleRenderElement::from_element(
            elem,
            center.to_physical_precise_round(scale),
            (progress / 2. + 0.5).max(0.),
        );

        let elem = RelocateRenderElement::from_element(
            elem,
            location.to_physical_precise_round(scale),
            Relocate::Relative,
        );

        Ok((elem.into(), data))
    }
}
