use std::sync::Arc;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::format::get_bpp;
use smithay::utils::{Buffer, Logical, Scale, Size, Transform};

#[derive(Clone)]
pub struct MemoryBuffer {
    data: Arc<[u8]>,
    format: Fourcc,
    size: Size<i32, Buffer>,
    scale: Scale<f64>,
    transform: Transform,
}

impl MemoryBuffer {
    /// # Panics
    ///
    /// Panics if `format`'s bits-per-pixel is unknown, or if `data` is smaller than
    /// `size.h * stride` bytes.
    pub fn new(
        data: impl Into<Arc<[u8]>>,
        format: Fourcc,
        size: impl Into<Size<i32, Buffer>>,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
    ) -> Self {
        let data = data.into();

        let size = size.into();
        let stride =
            size.w * (get_bpp(format).expect("Format with unknown bits per pixel") / 8) as i32;
        assert!(data.len() >= (stride * size.h) as usize);

        Self {
            data,
            format,
            size,
            scale: scale.into(),
            transform,
        }
    }

    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    #[must_use]
    pub const fn format(&self) -> Fourcc {
        self.format
    }

    #[must_use]
    pub const fn size(&self) -> Size<i32, Buffer> {
        self.size
    }

    #[must_use]
    pub const fn scale(&self) -> Scale<f64> {
        self.scale
    }

    #[must_use]
    pub const fn transform(&self) -> Transform {
        self.transform
    }

    #[must_use]
    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.size.to_f64().to_logical(self.scale, self.transform)
    }
}
