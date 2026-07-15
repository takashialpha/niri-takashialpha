use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use niri_config::{Config, ModKey};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::niri::Niri;
use crate::utils::id::IdCounter;

pub mod tty;
pub use tty::Tty;

pub mod headless;
pub use headless::Headless;

// Tty is much larger than Headless, but Headless only exists for tests, so boxing
// it to shrink Backend would just add a needless indirection to the hot (Tty) path.
#[allow(clippy::large_enum_variant)]
pub enum Backend {
    Tty(Tty),
    Headless(Headless),
}

#[derive(PartialEq, Eq)]
pub enum RenderResult {
    /// The frame was submitted to the backend for presentation.
    Submitted,
    /// Rendering succeeded, but there was no damage.
    NoDamage,
    /// The frame was not rendered and submitted, due to an error or otherwise.
    Skipped,
}

pub type IpcOutputMap = HashMap<OutputId, niri_ipc::Output>;

static OUTPUT_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(u64);

impl OutputId {
    fn next() -> Self {
        Self(OUTPUT_ID_COUNTER.next())
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Backend {
    pub fn init(&mut self, niri: &mut Niri) {
        match self {
            Self::Tty(tty) => tty.init(niri),
            Self::Headless(headless) => headless.init(niri),
        }
    }

    pub fn seat_name(&self) -> String {
        match self {
            Self::Tty(tty) => tty.seat_name(),
            Self::Headless(headless) => headless.seat_name(),
        }
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        match self {
            Self::Tty(tty) => tty.with_primary_renderer(f),
            Self::Headless(headless) => headless.with_primary_renderer(f),
        }
    }

    pub fn render(
        &mut self,
        niri: &mut Niri,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        match self {
            Self::Tty(tty) => tty.render(niri, output, target_presentation_time),
            Self::Headless(headless) => headless.render(niri, output),
        }
    }

    pub fn mod_key(&self, config: &Config) -> ModKey {
        config.input.mod_key.unwrap_or(ModKey::Super)
    }

    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Self::Tty(tty) => tty.change_vt(vt),
            Self::Headless(_) => (),
        }
    }

    pub const fn suspend(&mut self) {
        match self {
            Self::Tty(tty) => tty.suspend(),
            Self::Headless(_) => (),
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self {
            Self::Tty(tty) => tty.import_dmabuf(dmabuf),
            Self::Headless(headless) => headless.import_dmabuf(dmabuf),
        }
    }

    pub fn early_import(&mut self, surface: &WlSurface) {
        match self {
            Self::Tty(tty) => tty.early_import(surface),
            Self::Headless(_) => (),
        }
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        match self {
            Self::Tty(tty) => tty.ipc_outputs(),
            Self::Headless(headless) => headless.ipc_outputs(),
        }
    }

    pub fn set_monitors_active(&mut self, active: bool) {
        match self {
            Self::Tty(tty) => tty.set_monitors_active(active),
            Self::Headless(_) => (),
        }
    }

    pub fn on_output_config_changed(&mut self, niri: &mut Niri) {
        match self {
            Self::Tty(tty) => tty.on_output_config_changed(niri),
            Self::Headless(_) => (),
        }
    }

    pub const fn tty_checked(&mut self) -> Option<&mut Tty> {
        if let Self::Tty(v) = self {
            Some(v)
        } else {
            None
        }
    }

    /// # Panics
    ///
    /// Panics if the backend is not [`Self::Tty`].
    pub fn tty(&mut self) -> &mut Tty {
        if let Self::Tty(v) = self {
            v
        } else {
            panic!("backend is not Tty");
        }
    }

    /// # Panics
    ///
    /// Panics if the backend is not [`Self::Headless`].
    pub fn headless(&mut self) -> &mut Headless {
        if let Self::Headless(v) = self {
            v
        } else {
            panic!("backend is not Headless")
        }
    }
}
