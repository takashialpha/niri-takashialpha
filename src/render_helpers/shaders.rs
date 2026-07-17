use std::cell::RefCell;

use glam::Mat3;
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
    UniformValue,
};

use super::renderer::NiriRenderer;
use super::shader_element::ShaderProgram;

pub struct Shaders {
    pub border: Option<ShaderProgram>,
    pub shadow: Option<ShaderProgram>,
    pub clipped_surface: Option<GlesTexProgram>,
    pub postprocess_and_clip: Option<GlesTexProgram>,
    pub resize: Option<ShaderProgram>,
    pub gradient_fade: Option<GlesTexProgram>,
    pub custom_resize: RefCell<Option<ShaderProgram>>,
    pub custom_close: RefCell<Option<ShaderProgram>>,
    pub custom_open: RefCell<Option<ShaderProgram>>,
}

#[derive(Debug, Clone, Copy)]
pub enum ProgramType {
    Border,
    Shadow,
    Resize,
    Close,
    Open,
}

/// Logs a compile error and returns `None`, or unwraps the compiled program.
fn compiled_or_warn<T>(result: Result<T, GlesError>, what: &str) -> Option<T> {
    result
        .map_err(|err| warn!("error compiling {what} shader: {err:?}"))
        .ok()
}

impl Shaders {
    // Flat sequential "compile this shader, then that one" initialization with no
    // control flow; it's long only because each shader's uniform list is spelled
    // out, not because it's complex.
    #[allow(clippy::too_many_lines)]
    fn compile(renderer: &mut GlesRenderer) -> Self {
        let border = compiled_or_warn(
            ShaderProgram::compile(
                renderer,
                concat!(
                    include_str!("shaders/border.frag"),
                    include_str!("shaders/rounding_alpha.frag")
                ),
                &[
                    UniformName::new("colorspace", UniformType::_1f),
                    UniformName::new("hue_interpolation", UniformType::_1f),
                    UniformName::new("color_from", UniformType::_4f),
                    UniformName::new("color_to", UniformType::_4f),
                    UniformName::new("grad_offset", UniformType::_2f),
                    UniformName::new("grad_width", UniformType::_1f),
                    UniformName::new("grad_vec", UniformType::_2f),
                    UniformName::new("input_to_geo", UniformType::Matrix3x3),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("outer_radius", UniformType::_4f),
                    UniformName::new("border_width", UniformType::_1f),
                ],
                &[],
            ),
            "border",
        );

        let shadow = compiled_or_warn(
            ShaderProgram::compile(
                renderer,
                concat!(
                    include_str!("shaders/shadow.frag"),
                    include_str!("shaders/rounding_alpha.frag")
                ),
                &[
                    UniformName::new("shadow_color", UniformType::_4f),
                    UniformName::new("sigma", UniformType::_1f),
                    UniformName::new("input_to_geo", UniformType::Matrix3x3),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("corner_radius", UniformType::_4f),
                    UniformName::new("window_input_to_geo", UniformType::Matrix3x3),
                    UniformName::new("window_geo_size", UniformType::_2f),
                    UniformName::new("window_corner_radius", UniformType::_4f),
                ],
                &[],
            ),
            "shadow",
        );

        let clipped_surface = compiled_or_warn(
            renderer.compile_custom_texture_shader(
                concat!(
                    include_str!("shaders/clipped_surface.frag"),
                    include_str!("shaders/rounding_alpha.frag"),
                    "\nvec4 postprocess(vec4 color) { return color; }",
                ),
                &[
                    UniformName::new("niri_scale", UniformType::_1f),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("corner_radius", UniformType::_4f),
                    UniformName::new("input_to_geo", UniformType::Matrix3x3),
                ],
            ),
            "clipped surface",
        );

        let postprocess_and_clip = compiled_or_warn(
            renderer.compile_custom_texture_shader(
                concat!(
                    include_str!("shaders/clipped_surface.frag"),
                    include_str!("shaders/rounding_alpha.frag"),
                    include_str!("shaders/postprocess.frag"),
                ),
                &[
                    UniformName::new("niri_scale", UniformType::_1f),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("corner_radius", UniformType::_4f),
                    UniformName::new("input_to_geo", UniformType::Matrix3x3),
                    UniformName::new("noise", UniformType::_1f),
                    UniformName::new("saturation", UniformType::_1f),
                    UniformName::new("bg_color", UniformType::_4f),
                ],
            ),
            "postprocess_and_clip",
        );

        let resize = compiled_or_warn(
            compile_resize_program(renderer, include_str!("shaders/resize.frag")),
            "resize",
        );

        let gradient_fade = compiled_or_warn(
            renderer.compile_custom_texture_shader(
                include_str!("shaders/gradient_fade.frag"),
                &[UniformName::new("cutoff", UniformType::_2f)],
            ),
            "gradient fade",
        );

        Self {
            border,
            shadow,
            clipped_surface,
            postprocess_and_clip,
            resize,
            gradient_fade,
            custom_resize: RefCell::new(None),
            custom_close: RefCell::new(None),
            custom_open: RefCell::new(None),
        }
    }

    /// # Panics
    ///
    /// Panics if [`init()`] was not called on this renderer's EGL context before this is
    /// called.
    pub fn get_from_frame<'a>(frame: &'a mut GlesFrame<'_, '_>) -> &'a Self {
        let data = frame.egl_context().user_data();
        data.get()
            .expect("shaders::init() must be called when creating the renderer")
    }

    /// # Panics
    ///
    /// Panics if [`init()`] was not called on this renderer's EGL context before this is
    /// called.
    pub fn get(renderer: &mut impl NiriRenderer) -> &Self {
        let renderer = renderer.as_gles_renderer();
        let data = renderer.egl_context().user_data();
        data.get()
            .expect("shaders::init() must be called when creating the renderer")
    }

    pub fn replace_custom_resize_program(
        &self,
        program: Option<ShaderProgram>,
    ) -> Option<ShaderProgram> {
        self.custom_resize.replace(program)
    }

    pub fn replace_custom_close_program(
        &self,
        program: Option<ShaderProgram>,
    ) -> Option<ShaderProgram> {
        self.custom_close.replace(program)
    }

    pub fn replace_custom_open_program(
        &self,
        program: Option<ShaderProgram>,
    ) -> Option<ShaderProgram> {
        self.custom_open.replace(program)
    }

    pub fn program(&self, program: ProgramType) -> Option<ShaderProgram> {
        match program {
            ProgramType::Border => self.border.clone(),
            ProgramType::Shadow => self.shadow.clone(),
            ProgramType::Resize => self
                .custom_resize
                .borrow()
                .clone()
                .or_else(|| self.resize.clone()),
            ProgramType::Close => self.custom_close.borrow().clone(),
            ProgramType::Open => self.custom_open.borrow().clone(),
        }
    }
}

pub fn init(renderer: &mut GlesRenderer) {
    let shaders = Shaders::compile(renderer);
    let data = renderer.egl_context().user_data();
    if !data.insert_if_missing(|| shaders) {
        error!("shaders were already compiled");
    }
}

fn compile_resize_program(
    renderer: &mut GlesRenderer,
    src: &str,
) -> Result<ShaderProgram, GlesError> {
    let mut program = include_str!("shaders/resize_prelude.frag").to_string();
    program.push_str(src);
    program.push_str(include_str!("shaders/resize_epilogue.frag"));
    program.push_str(include_str!("shaders/rounding_alpha.frag"));

    ShaderProgram::compile(
        renderer,
        &program,
        &[
            UniformName::new("niri_input_to_curr_geo", UniformType::Matrix3x3),
            UniformName::new("niri_curr_geo_to_prev_geo", UniformType::Matrix3x3),
            UniformName::new("niri_curr_geo_to_next_geo", UniformType::Matrix3x3),
            UniformName::new("niri_curr_geo_size", UniformType::_2f),
            UniformName::new("niri_geo_to_tex_prev", UniformType::Matrix3x3),
            UniformName::new("niri_geo_to_tex_next", UniformType::Matrix3x3),
            UniformName::new("niri_progress", UniformType::_1f),
            UniformName::new("niri_clamped_progress", UniformType::_1f),
            UniformName::new("niri_corner_radius", UniformType::_4f),
            UniformName::new("niri_clip_to_geometry", UniformType::_1f),
        ],
        &["niri_tex_prev", "niri_tex_next"],
    )
}

pub fn set_custom_resize_program(renderer: &mut GlesRenderer, src: Option<&str>) {
    let program = if let Some(src) = src {
        match compile_resize_program(renderer, src) {
            Ok(program) => Some(program),
            Err(err) => {
                warn!("error compiling custom resize shader: {err:?}");
                return;
            }
        }
    } else {
        None
    };

    if let Some(prev) = Shaders::get(renderer).replace_custom_resize_program(program)
        && let Err(err) = prev.destroy(renderer)
    {
        warn!("error destroying previous custom resize shader: {err:?}");
    }
}

fn compile_close_program(
    renderer: &mut GlesRenderer,
    src: &str,
) -> Result<ShaderProgram, GlesError> {
    let mut program = include_str!("shaders/close_prelude.frag").to_string();
    program.push_str(src);
    program.push_str(include_str!("shaders/close_epilogue.frag"));

    ShaderProgram::compile(
        renderer,
        &program,
        &[
            UniformName::new("niri_input_to_geo", UniformType::Matrix3x3),
            UniformName::new("niri_geo_size", UniformType::_2f),
            UniformName::new("niri_geo_to_tex", UniformType::Matrix3x3),
            UniformName::new("niri_progress", UniformType::_1f),
            UniformName::new("niri_clamped_progress", UniformType::_1f),
            UniformName::new("niri_random_seed", UniformType::_1f),
        ],
        &["niri_tex"],
    )
}

pub fn set_custom_close_program(renderer: &mut GlesRenderer, src: Option<&str>) {
    let program = if let Some(src) = src {
        match compile_close_program(renderer, src) {
            Ok(program) => Some(program),
            Err(err) => {
                warn!("error compiling custom close shader: {err:?}");
                return;
            }
        }
    } else {
        None
    };

    if let Some(prev) = Shaders::get(renderer).replace_custom_close_program(program)
        && let Err(err) = prev.destroy(renderer)
    {
        warn!("error destroying previous custom close shader: {err:?}");
    }
}

fn compile_open_program(
    renderer: &mut GlesRenderer,
    src: &str,
) -> Result<ShaderProgram, GlesError> {
    let mut program = include_str!("shaders/open_prelude.frag").to_string();
    program.push_str(src);
    program.push_str(include_str!("shaders/open_epilogue.frag"));

    ShaderProgram::compile(
        renderer,
        &program,
        &[
            UniformName::new("niri_input_to_geo", UniformType::Matrix3x3),
            UniformName::new("niri_geo_size", UniformType::_2f),
            UniformName::new("niri_geo_to_tex", UniformType::Matrix3x3),
            UniformName::new("niri_progress", UniformType::_1f),
            UniformName::new("niri_clamped_progress", UniformType::_1f),
            UniformName::new("niri_random_seed", UniformType::_1f),
        ],
        &["niri_tex"],
    )
}

pub fn set_custom_open_program(renderer: &mut GlesRenderer, src: Option<&str>) {
    let program = if let Some(src) = src {
        match compile_open_program(renderer, src) {
            Ok(program) => Some(program),
            Err(err) => {
                warn!("error compiling custom open shader: {err:?}");
                return;
            }
        }
    } else {
        None
    };

    if let Some(prev) = Shaders::get(renderer).replace_custom_open_program(program)
        && let Err(err) = prev.destroy(renderer)
    {
        warn!("error destroying previous custom open shader: {err:?}");
    }
}

#[must_use]
pub fn mat3_uniform(name: &str, mat: Mat3) -> Uniform<'_> {
    Uniform::new(
        name,
        UniformValue::Matrix3x3 {
            matrices: vec![mat.to_cols_array()],
            transpose: false,
        },
    )
}
