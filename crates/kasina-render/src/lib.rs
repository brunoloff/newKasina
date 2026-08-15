//! Retained custom wgpu renderer embedded in egui plus frame instrumentation.

use std::collections::VecDeque;
use std::mem::size_of;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use egui_wgpu::wgpu;
use egui_wgpu::wgpu::util::DeviceExt as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Rolling CPU frame and upload instrumentation.
#[derive(Debug, Clone)]
pub struct FrameStats {
    samples: VecDeque<Duration>,
    capacity: usize,
    uploaded_bytes: u64,
}

impl FrameStats {
    /// Create a fixed-size rolling statistics window.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
            uploaded_bytes: 0,
        }
    }

    /// Record one duration and the bytes uploaded during that operation.
    pub fn record(&mut self, duration: Duration, uploaded_bytes: u64) {
        if self.capacity == 0 {
            return;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(duration);
        self.uploaded_bytes = uploaded_bytes;
    }

    /// Number of measurements currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether no measurements have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Average duration.
    #[must_use]
    pub fn average(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let total = self.samples.iter().map(Duration::as_nanos).sum::<u128>();
        Duration::from_nanos((total / self.samples.len() as u128).min(u128::from(u64::MAX)) as u64)
    }

    /// Nearest-rank percentile from the current window.
    #[must_use]
    pub fn percentile(&self, percentile: f64) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let mut values: Vec<_> = self.samples.iter().copied().collect();
        values.sort_unstable();
        let rank = (percentile.clamp(0.0, 1.0) * (values.len() - 1) as f64).round() as usize;
        values[rank]
    }

    /// Bytes uploaded during the latest operation.
    #[must_use]
    pub const fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new(240)
    }
}

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct VisualUniforms {
    time_seconds: f32,
    respiration: f32,
    instance_count: u32,
    style: u32,
    viewport_points: [f32; 2],
    radius_range: [f32; 2],
    layer_rotation_radians: [f32; 4],
    effect_params: [f32; 4],
}

const PARTICLE_STYLE: u32 = 0;
const BREATH_KASINA_STYLE: u32 = 1;
const AURORA_VORTEX_STYLE: u32 = 2;
const ORGANIC_KALEIDOSCOPE_STYLE: u32 = 3;

/// Slowest selectable kasina animation rate, in complete cycles per second.
pub const MIN_ROTATIONS_PER_SECOND: f32 = 0.01;
/// Fastest selectable kasina animation rate, in complete cycles per second.
pub const MAX_ROTATIONS_PER_SECOND: f32 = 10.0;
/// Smallest selectable full-expansion speed multiplier.
pub const MIN_EXPANSION_SPEED_MULTIPLIER: f32 = 1.0;
/// Largest selectable full-expansion speed multiplier.
pub const MAX_EXPANSION_SPEED_MULTIPLIER: f32 = 10.0;

/// CPU-side input prepared for one biofeedback draw.
///
/// Construction performs all normalization needed before the callback reaches wgpu. The
/// resulting uniform upload is fixed at 64 bytes regardless of the instance count.
#[derive(Debug, Clone, Copy)]
pub struct PreparedVisualFrame {
    uniforms: VisualUniforms,
}

struct KasinaUniformInput {
    style: u32,
    animation_state: f32,
    auxiliary_state: u32,
    layer_rotation_radians: [f32; 4],
    respiration: f32,
    viewport_points: [f32; 2],
    radius_range: [f32; 2],
    effect_params: [f32; 4],
}

impl PreparedVisualFrame {
    /// Normalize one frame of visual state into the exact GPU uniform layout.
    #[must_use]
    pub fn new(
        time_seconds: f32,
        respiration: f32,
        instance_count: u32,
        viewport_points: [f32; 2],
    ) -> Self {
        Self {
            uniforms: VisualUniforms {
                time_seconds,
                respiration: respiration.clamp(0.0, 1.0),
                instance_count: instance_count.max(1),
                style: PARTICLE_STYLE,
                viewport_points: [viewport_points[0].max(1.0), viewport_points[1].max(1.0)],
                radius_range: [0.0; 2],
                layer_rotation_radians: [0.0; 4],
                effect_params: [0.0; 4],
            },
        }
    }

    fn kasina(input: KasinaUniformInput) -> Self {
        Self {
            uniforms: VisualUniforms {
                time_seconds: input.animation_state,
                respiration: input.respiration.clamp(0.0, 1.0),
                instance_count: input.auxiliary_state,
                style: input.style,
                viewport_points: [
                    input.viewport_points[0].max(1.0),
                    input.viewport_points[1].max(1.0),
                ],
                radius_range: input.radius_range,
                layer_rotation_radians: input.layer_rotation_radians,
                effect_params: input.effect_params,
            },
        }
    }

    /// Exact bytes queued to the retained uniform buffer.
    #[must_use]
    pub fn upload_bytes(&self) -> &[u8] {
        bytemuck::bytes_of(&self.uniforms)
    }

    /// Number of instances emitted by the draw call.
    #[must_use]
    pub const fn instance_count(&self) -> u32 {
        if self.uniforms.style == PARTICLE_STYLE {
            self.uniforms.instance_count
        } else {
            1
        }
    }
}

/// Inputs common to every breath-driven kasina implementation.
#[derive(Debug, Clone, Copy)]
pub struct KasinaFrameInput {
    /// Independently integrated layer phases, in complete rotations.
    pub layer_rotation_phases: [f32; 4],
    /// Smoothed expansion in the inclusive range zero to one.
    pub respiration: f32,
    /// Available logical viewport size.
    pub viewport_points: [f32; 2],
    /// Monotonic generation assigned whenever a new inhale begins.
    pub breath_generation: u32,
    /// Whether the latest confidently classified breath direction is inward.
    pub inhaling: bool,
}

/// Transient kasina state supplied by the app before the renderer knows its viewport.
#[derive(Debug, Clone, Copy)]
pub struct KasinaAnimationInput {
    /// Independently integrated layer phases, in complete rotations.
    pub layer_rotation_phases: [f32; 4],
    /// Smoothed expansion in the inclusive range zero to one.
    pub respiration: f32,
    /// Monotonic generation assigned whenever a new inhale begins.
    pub breath_generation: u32,
    /// Whether the latest confidently classified breath direction is inward.
    pub inhaling: bool,
}

/// A renderable kasina implementation.
///
/// Implementations own their typed options and reduce them to the retained renderer's
/// small per-frame uniform payload. Presets select implementations separately from this
/// rendering interface so future shapes do not leak into the application state model.
pub trait KasinaVisual: std::fmt::Debug + Send + Sync {
    /// Stable identifier stored by presets.
    fn implementation_id(&self) -> &'static str;
    /// User-facing implementation name.
    fn display_name(&self) -> &'static str;
    /// Return four independently integrated animation speeds for this frame.
    fn layer_speeds(&self, expansion: f32) -> [f32; 4];
    /// Prepare one constant-size GPU update.
    fn prepare_frame(&self, input: KasinaFrameInput) -> PreparedVisualFrame;
}

/// Options for the original luminous circular mandala.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LuminousMandala {
    /// Radius at the bottom of the calibrated breathing range.
    pub minimum_radius: f32,
    /// Radius at the top of the calibrated breathing range.
    pub maximum_radius: f32,
    /// Whether the lace pattern rotates.
    pub rotation_enabled: bool,
    /// Contracted rotation speed of the inner flower, in complete rotations per second.
    pub inner_rotations_per_second: f32,
    /// Contracted rotation speed of the middle flower, in complete rotations per second.
    pub middle_rotations_per_second: f32,
    /// Contracted rotation speed of the third flower, in complete rotations per second.
    pub third_rotations_per_second: f32,
    /// Contracted rotation speed of the outer gold ring, in complete rotations per second.
    pub gold_rotations_per_second: f32,
    /// Speed multiplier reached at full expansion. One disables breath modulation.
    pub expansion_speed_multiplier: f32,
    /// Pre-schema-4 shared rotation speed retained only while loading old settings.
    #[doc(hidden)]
    #[serde(
        rename = "rotations_per_second",
        alias = "rotation_speed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_rotations_per_second: Option<f32>,
}

impl LuminousMandala {
    /// Clamp settings loaded from disk or edited by a user to safe visual bounds.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.minimum_radius = self.minimum_radius.clamp(0.12, 0.80);
        self.maximum_radius = self.maximum_radius.clamp(0.20, 1.00);
        if self.maximum_radius < self.minimum_radius + 0.05 {
            self.maximum_radius = (self.minimum_radius + 0.05).min(1.00);
            self.minimum_radius = self.minimum_radius.min(self.maximum_radius - 0.05);
        }
        self.inner_rotations_per_second = sanitize_rotation_rate(self.inner_rotations_per_second);
        self.middle_rotations_per_second = sanitize_rotation_rate(self.middle_rotations_per_second);
        self.third_rotations_per_second = sanitize_rotation_rate(self.third_rotations_per_second);
        self.gold_rotations_per_second = sanitize_rotation_rate(self.gold_rotations_per_second);
        self.expansion_speed_multiplier = self.expansion_speed_multiplier.clamp(
            MIN_EXPANSION_SPEED_MULTIPLIER,
            MAX_EXPANSION_SPEED_MULTIPLIER,
        );
        self.legacy_rotations_per_second = None;
        self
    }

    /// Expand a legacy shared speed into the four independently configurable layers.
    pub fn migrate_shared_rotation_speed(&mut self, value_was_radians_per_second: bool) {
        let Some(mut speed) = self.legacy_rotations_per_second.take() else {
            return;
        };
        if value_was_radians_per_second {
            speed /= std::f32::consts::TAU;
        }
        self.inner_rotations_per_second = speed;
        self.middle_rotations_per_second = speed;
        self.third_rotations_per_second = speed;
        self.gold_rotations_per_second = speed;
    }

    /// Return the four instantaneous speeds for the current breath expansion.
    #[must_use]
    pub fn layer_speeds(&self, expansion: f32) -> [f32; 4] {
        let options = self.sanitized();
        if !options.rotation_enabled {
            return [0.0; 4];
        }
        modulated_layer_speeds(
            [
                options.inner_rotations_per_second,
                options.middle_rotations_per_second,
                options.third_rotations_per_second,
                options.gold_rotations_per_second,
            ],
            options.expansion_speed_multiplier,
            expansion,
        )
    }
}

fn sanitize_rotation_rate(speed: f32) -> f32 {
    speed.clamp(MIN_ROTATIONS_PER_SECOND, MAX_ROTATIONS_PER_SECOND)
}

fn modulated_layer_speeds(
    base_speeds: [f32; 4],
    expansion_speed_multiplier: f32,
    expansion: f32,
) -> [f32; 4] {
    let factor = 1.0
        + expansion.clamp(0.0, 1.0)
            * (expansion_speed_multiplier.clamp(
                MIN_EXPANSION_SPEED_MULTIPLIER,
                MAX_EXPANSION_SPEED_MULTIPLIER,
            ) - 1.0);
    base_speeds.map(|speed| (speed * factor).min(MAX_ROTATIONS_PER_SECOND))
}

impl Default for LuminousMandala {
    fn default() -> Self {
        Self {
            minimum_radius: 0.40,
            maximum_radius: 0.82,
            rotation_enabled: true,
            inner_rotations_per_second: 0.05,
            middle_rotations_per_second: 0.05,
            third_rotations_per_second: 0.05,
            gold_rotations_per_second: 0.05,
            expansion_speed_multiplier: 2.0,
            legacy_rotations_per_second: None,
        }
    }
}

impl KasinaVisual for LuminousMandala {
    fn implementation_id(&self) -> &'static str {
        "luminous-mandala"
    }

    fn display_name(&self) -> &'static str {
        "Luminous mandala"
    }

    fn layer_speeds(&self, expansion: f32) -> [f32; 4] {
        LuminousMandala::layer_speeds(self, expansion)
    }

    fn prepare_frame(&self, input: KasinaFrameInput) -> PreparedVisualFrame {
        let options = self.sanitized();
        let layer_rotation_radians = if options.rotation_enabled {
            input
                .layer_rotation_phases
                .map(|phase| phase.rem_euclid(1.0) * std::f32::consts::TAU)
        } else {
            [0.0; 4]
        };
        PreparedVisualFrame::kasina(KasinaUniformInput {
            style: BREATH_KASINA_STYLE,
            animation_state: 0.0,
            auxiliary_state: 1,
            layer_rotation_radians,
            respiration: input.respiration,
            viewport_points: input.viewport_points,
            radius_range: [options.minimum_radius, options.maximum_radius],
            effect_params: [0.0; 4],
        })
    }
}

/// A prismatic spiral field inspired by aurora curtains and cymatic interference.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuroraVortex {
    /// Radius at the bottom of the calibrated breathing range.
    pub minimum_radius: f32,
    /// Radius at the top of the calibrated breathing range.
    pub maximum_radius: f32,
    /// Whether the vortex layers rotate.
    pub rotation_enabled: bool,
    /// Contracted rotation speed of the breathing iris.
    pub iris_rotations_per_second: f32,
    /// Contracted rotation speed of the opposing filament field.
    pub filament_rotations_per_second: f32,
    /// Contracted rotation speed of the outer halo.
    pub halo_rotations_per_second: f32,
    /// Contracted rotation speed of the spark orbit.
    pub spark_rotations_per_second: f32,
    /// Speed multiplier reached at full expansion. One disables breath modulation.
    pub expansion_speed_multiplier: f32,
    /// Number of spiral arms.
    pub arms: u32,
    /// Logarithmic spiral winding amount.
    pub twist: f32,
    /// Intensity of the luminous lines and aura.
    pub glow: f32,
    /// Position around the spectral color wheel, from zero to one.
    pub hue: f32,
}

impl AuroraVortex {
    /// Clamp loaded or edited settings to stable visual and performance bounds.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.minimum_radius = self.minimum_radius.clamp(0.12, 0.80);
        self.maximum_radius = self.maximum_radius.clamp(0.20, 1.00);
        if self.maximum_radius < self.minimum_radius + 0.05 {
            self.maximum_radius = (self.minimum_radius + 0.05).min(1.00);
            self.minimum_radius = self.minimum_radius.min(self.maximum_radius - 0.05);
        }
        self.iris_rotations_per_second = sanitize_rotation_rate(self.iris_rotations_per_second);
        self.filament_rotations_per_second =
            sanitize_rotation_rate(self.filament_rotations_per_second);
        self.halo_rotations_per_second = sanitize_rotation_rate(self.halo_rotations_per_second);
        self.spark_rotations_per_second = sanitize_rotation_rate(self.spark_rotations_per_second);
        self.expansion_speed_multiplier = self.expansion_speed_multiplier.clamp(
            MIN_EXPANSION_SPEED_MULTIPLIER,
            MAX_EXPANSION_SPEED_MULTIPLIER,
        );
        self.arms = self.arms.clamp(3, 24);
        self.twist = self.twist.clamp(1.0, 14.0);
        self.glow = self.glow.clamp(0.35, 2.50);
        self.hue = self.hue.rem_euclid(1.0);
        self
    }

    /// Return the four instantaneous vortex speeds for the current breath expansion.
    #[must_use]
    pub fn layer_speeds(&self, expansion: f32) -> [f32; 4] {
        let options = self.sanitized();
        if !options.rotation_enabled {
            return [0.0; 4];
        }
        modulated_layer_speeds(
            [
                options.iris_rotations_per_second,
                options.filament_rotations_per_second,
                options.halo_rotations_per_second,
                options.spark_rotations_per_second,
            ],
            options.expansion_speed_multiplier,
            expansion,
        )
    }
}

impl Default for AuroraVortex {
    fn default() -> Self {
        Self {
            minimum_radius: 0.34,
            maximum_radius: 0.88,
            rotation_enabled: true,
            iris_rotations_per_second: 0.035,
            filament_rotations_per_second: 0.022,
            halo_rotations_per_second: 0.014,
            spark_rotations_per_second: 0.055,
            expansion_speed_multiplier: 2.8,
            arms: 9,
            twist: 7.5,
            glow: 1.25,
            hue: 0.54,
        }
    }
}

impl KasinaVisual for AuroraVortex {
    fn implementation_id(&self) -> &'static str {
        "aurora-vortex"
    }

    fn display_name(&self) -> &'static str {
        "Aurora vortex"
    }

    fn layer_speeds(&self, expansion: f32) -> [f32; 4] {
        AuroraVortex::layer_speeds(self, expansion)
    }

    fn prepare_frame(&self, input: KasinaFrameInput) -> PreparedVisualFrame {
        let options = self.sanitized();
        let layer_rotation_radians = if options.rotation_enabled {
            input
                .layer_rotation_phases
                .map(|phase| phase.rem_euclid(1.0) * std::f32::consts::TAU)
        } else {
            [0.0; 4]
        };
        PreparedVisualFrame::kasina(KasinaUniformInput {
            style: AURORA_VORTEX_STYLE,
            animation_state: 0.0,
            auxiliary_state: 1,
            layer_rotation_radians,
            respiration: input.respiration,
            viewport_points: input.viewport_points,
            radius_range: [options.minimum_radius, options.maximum_radius],
            effect_params: [
                options.arms as f32,
                options.twist,
                options.glow,
                options.hue,
            ],
        })
    }
}

/// A continuously evolving radial kaleidoscope that accumulates one color layer per breath.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrganicKaleidoscope {
    /// Radius of the new color seed born when an inhale begins.
    #[serde(alias = "minimum_aperture_radius")]
    pub seed_radius: f32,
    /// Width occupied by one completed breath layer.
    #[serde(alias = "maximum_aperture_radius")]
    pub completed_layer_width: f32,
    /// Whether the four independent animation channels advance.
    pub animation_enabled: bool,
    /// Rotation speed of the mirrored wedge geometry.
    pub geometry_rotations_per_second: f32,
    /// Evolution speed of the underlying organic field.
    pub morph_rotations_per_second: f32,
    /// Speed of the color-palette drift.
    pub palette_rotations_per_second: f32,
    /// Evolution speed of the coordinate warping.
    pub warp_rotations_per_second: f32,
    /// Animation multiplier reached at full expansion. One disables breath modulation.
    pub expansion_speed_multiplier: f32,
    /// Number of mirrored radial sectors.
    pub sectors: u32,
    /// Density of the nested rings and material contours.
    pub ring_density: f32,
    /// Strength of the slow organic coordinate distortion.
    pub warp: f32,
    /// Position around the base color palette, from zero to one.
    pub hue: f32,
}

impl OrganicKaleidoscope {
    /// Clamp loaded or edited settings to stable visual and performance bounds.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.seed_radius = self.seed_radius.clamp(0.005, 0.15);
        self.completed_layer_width = self.completed_layer_width.clamp(0.06, 0.35);
        if self.completed_layer_width < self.seed_radius + 0.02 {
            self.completed_layer_width = (self.seed_radius + 0.02).min(0.35);
            self.seed_radius = self.seed_radius.min(self.completed_layer_width - 0.02);
        }
        self.geometry_rotations_per_second =
            sanitize_rotation_rate(self.geometry_rotations_per_second);
        self.morph_rotations_per_second = sanitize_rotation_rate(self.morph_rotations_per_second);
        self.palette_rotations_per_second =
            sanitize_rotation_rate(self.palette_rotations_per_second);
        self.warp_rotations_per_second = sanitize_rotation_rate(self.warp_rotations_per_second);
        self.expansion_speed_multiplier = self.expansion_speed_multiplier.clamp(
            MIN_EXPANSION_SPEED_MULTIPLIER,
            MAX_EXPANSION_SPEED_MULTIPLIER,
        );
        self.sectors = self.sectors.clamp(4, 32);
        self.ring_density = self.ring_density.clamp(2.0, 14.0);
        self.warp = self.warp.clamp(0.0, 1.50);
        self.hue = self.hue.rem_euclid(1.0);
        self
    }

    /// Return the four instantaneous kaleidoscope animation speeds.
    #[must_use]
    pub fn layer_speeds(&self, expansion: f32) -> [f32; 4] {
        let options = self.sanitized();
        if !options.animation_enabled {
            return [0.0; 4];
        }
        modulated_layer_speeds(
            [
                options.geometry_rotations_per_second,
                options.morph_rotations_per_second,
                options.palette_rotations_per_second,
                options.warp_rotations_per_second,
            ],
            options.expansion_speed_multiplier,
            expansion,
        )
    }
}

impl Default for OrganicKaleidoscope {
    fn default() -> Self {
        Self {
            seed_radius: 0.028,
            completed_layer_width: 0.19,
            animation_enabled: true,
            geometry_rotations_per_second: 0.012,
            morph_rotations_per_second: 0.018,
            palette_rotations_per_second: 0.010,
            warp_rotations_per_second: 0.014,
            expansion_speed_multiplier: 1.6,
            sectors: 18,
            ring_density: 6.5,
            warp: 0.82,
            hue: 0.06,
        }
    }
}

impl KasinaVisual for OrganicKaleidoscope {
    fn implementation_id(&self) -> &'static str {
        "organic-kaleidoscope"
    }

    fn display_name(&self) -> &'static str {
        "Organic kaleidoscope"
    }

    fn layer_speeds(&self, expansion: f32) -> [f32; 4] {
        OrganicKaleidoscope::layer_speeds(self, expansion)
    }

    fn prepare_frame(&self, input: KasinaFrameInput) -> PreparedVisualFrame {
        let options = self.sanitized();
        let layer_rotation_radians = if options.animation_enabled {
            input
                .layer_rotation_phases
                .map(|phase| phase.rem_euclid(1.0) * std::f32::consts::TAU)
        } else {
            [0.0; 4]
        };
        let breath_phase = if input.inhaling {
            input.respiration.clamp(0.0, 1.0) * 0.49
        } else {
            0.50 + input.respiration.clamp(0.0, 1.0) * 0.49
        };
        PreparedVisualFrame::kasina(KasinaUniformInput {
            style: ORGANIC_KALEIDOSCOPE_STYLE,
            animation_state: breath_phase,
            auxiliary_state: input.breath_generation,
            layer_rotation_radians,
            respiration: input.respiration,
            viewport_points: input.viewport_points,
            radius_range: [options.seed_radius, options.completed_layer_width],
            effect_params: [
                options.sectors as f32,
                options.ring_density,
                options.warp,
                options.hue,
            ],
        })
    }
}

struct BiofeedbackResources {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
}

/// Retained renderer installed into eframe's shared callback resource map.
#[derive(Clone)]
pub struct BiofeedbackRenderer {
    prepare_stats: Arc<Mutex<FrameStats>>,
}

impl std::fmt::Debug for BiofeedbackRenderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BiofeedbackRenderer")
            .finish_non_exhaustive()
    }
}

impl BiofeedbackRenderer {
    /// Create the wgpu pipeline once and retain it for the application's lifetime.
    #[must_use]
    pub fn new(render_state: &egui_wgpu::RenderState) -> Self {
        let device = &render_state.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("newKasina biofeedback shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("biofeedback.wgsl").into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("newKasina biofeedback bind-group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size_of::<VisualUniforms>() as u64),
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("newKasina biofeedback pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("newKasina biofeedback pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: render_state.target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let initial_uniforms = VisualUniforms {
            time_seconds: 0.0,
            respiration: 0.5,
            instance_count: 1,
            style: PARTICLE_STYLE,
            viewport_points: [1.0, 1.0],
            radius_range: [0.0; 2],
            layer_rotation_radians: [0.0; 4],
            effect_params: [0.0; 4],
        };
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("newKasina biofeedback uniforms"),
            contents: bytemuck::bytes_of(&initial_uniforms),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("newKasina biofeedback bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });
        render_state
            .renderer
            .write()
            .callback_resources
            .insert(BiofeedbackResources {
                pipeline,
                bind_group,
                uniform_buffer,
            });
        Self {
            prepare_stats: Arc::new(Mutex::new(FrameStats::default())),
        }
    }

    /// Add a custom wgpu paint callback for the available egui region.
    pub fn paint(
        &self,
        ui: &mut egui::Ui,
        desired_size: egui::Vec2,
        time_seconds: f32,
        respiration: f32,
        stress_instances: u32,
    ) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
        let prepared = PreparedVisualFrame::new(
            time_seconds,
            respiration,
            stress_instances,
            [rect.width(), rect.height()],
        );
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            rect,
            BiofeedbackCallback {
                uniforms: prepared.uniforms,
                prepare_stats: Arc::clone(&self.prepare_stats),
            },
        ));
        response
    }

    /// Add the analytic, force-driven breath kasina to the available egui region.
    pub fn paint_breath_kasina(
        &self,
        ui: &mut egui::Ui,
        desired_size: egui::Vec2,
        visual: &dyn KasinaVisual,
        animation: KasinaAnimationInput,
    ) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
        let prepared = visual.prepare_frame(KasinaFrameInput {
            layer_rotation_phases: animation.layer_rotation_phases,
            respiration: animation.respiration,
            viewport_points: [rect.width(), rect.height()],
            breath_generation: animation.breath_generation,
            inhaling: animation.inhaling,
        });
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            rect,
            BiofeedbackCallback {
                uniforms: prepared.uniforms,
                prepare_stats: Arc::clone(&self.prepare_stats),
            },
        ));
        response
    }

    /// Snapshot callback preparation timings and latest uniform upload size.
    #[must_use]
    pub fn prepare_stats(&self) -> FrameStats {
        self.prepare_stats.lock().clone()
    }
}

#[derive(Clone)]
struct BiofeedbackCallback {
    uniforms: VisualUniforms,
    prepare_stats: Arc<Mutex<FrameStats>>,
}

impl egui_wgpu::CallbackTrait for BiofeedbackCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let started = Instant::now();
        if let Some(resources) = resources.get::<BiofeedbackResources>() {
            queue.write_buffer(
                &resources.uniform_buffer,
                0,
                bytemuck::bytes_of(&self.uniforms),
            );
            self.prepare_stats
                .lock()
                .record(started.elapsed(), size_of::<VisualUniforms>() as u64);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(resources) = resources.get::<BiofeedbackResources>() else {
            return;
        };
        render_pass.set_pipeline(&resources.pipeline);
        render_pass.set_bind_group(0, &resources.bind_group, &[]);
        let instance_count = if self.uniforms.style == PARTICLE_STYLE {
            self.uniforms.instance_count
        } else {
            1
        };
        render_pass.draw(0..6, 0..instance_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_window_is_bounded_and_reports_percentiles() {
        let mut stats = FrameStats::new(3);
        for millis in 1..=4 {
            stats.record(Duration::from_millis(millis), millis);
        }
        assert_eq!(stats.len(), 3);
        assert_eq!(stats.average(), Duration::from_millis(3));
        assert_eq!(stats.percentile(0.95), Duration::from_millis(4));
        assert_eq!(stats.uploaded_bytes(), 4);
    }

    #[test]
    fn zero_sized_window_ignores_samples() {
        let mut stats = FrameStats::new(0);
        stats.record(Duration::from_millis(1), 10);
        assert!(stats.is_empty());
        assert_eq!(stats.uploaded_bytes(), 0);
    }

    #[test]
    fn biofeedback_shader_parses_without_a_gpu() {
        naga::front::wgsl::parse_str(include_str!("biofeedback.wgsl")).unwrap();
    }

    #[test]
    fn mandala_shader_alternates_four_independent_rotation_phases() {
        let shader = include_str!("biofeedback.wgsl");
        assert!(shader.contains("let inner_angle = angle - rotation.x"));
        assert!(shader.contains("let middle_angle = angle + rotation.y"));
        assert!(shader.contains("let third_angle = angle - rotation.z"));
        assert!(shader.contains("let gold_angle = angle + rotation.w"));
        assert!(shader.contains("cos(inner_angle * 8.0)"));
        assert!(shader.contains("cos(middle_angle * 12.0)"));
        assert!(shader.contains("cos(third_angle * 24.0)"));
        assert!(shader.contains("sin(gold_angle * 24.0 + radius * 6.0)"));
    }

    #[test]
    fn prepared_frame_clamps_inputs_and_has_a_fixed_upload() {
        let low = PreparedVisualFrame::new(2.0, -4.0, 0, [0.0, -10.0]);
        let high = PreparedVisualFrame::new(2.0, 4.0, 100_000, [1_920.0, 1_080.0]);

        assert_eq!(low.uniforms.respiration, 0.0);
        assert_eq!(low.uniforms.instance_count, 1);
        assert_eq!(low.uniforms.viewport_points, [1.0, 1.0]);
        assert_eq!(high.uniforms.respiration, 1.0);
        assert_eq!(high.uniforms.instance_count, 100_000);
        assert_eq!(low.upload_bytes().len(), 64);
        assert_eq!(high.upload_bytes().len(), low.upload_bytes().len());
    }

    #[test]
    fn breath_kasina_uses_one_full_screen_instance() {
        let frame = LuminousMandala::default().prepare_frame(KasinaFrameInput {
            layer_rotation_phases: [0.125, 0.25, 0.375, 0.5],
            respiration: 1.5,
            viewport_points: [900.0, 600.0],
            breath_generation: 0,
            inhaling: false,
        });

        assert_eq!(frame.uniforms.respiration, 1.0);
        assert_eq!(frame.uniforms.style, BREATH_KASINA_STYLE);
        assert_eq!(frame.uniforms.radius_range, [0.40, 0.82]);
        assert_eq!(frame.instance_count(), 1);
        assert_eq!(frame.upload_bytes().len(), 64);
        assert_eq!(
            frame.uniforms.layer_rotation_radians,
            [
                std::f32::consts::FRAC_PI_4,
                std::f32::consts::FRAC_PI_2,
                std::f32::consts::FRAC_PI_4 * 3.0,
                std::f32::consts::PI,
            ]
        );
    }

    #[test]
    fn luminous_mandala_sanitizes_options_and_can_disable_rotation() {
        let visual = LuminousMandala {
            minimum_radius: 2.0,
            maximum_radius: -1.0,
            rotation_enabled: false,
            inner_rotations_per_second: 40.0,
            expansion_speed_multiplier: 40.0,
            ..LuminousMandala::default()
        };
        let sanitized = visual.sanitized();
        let frame = visual.prepare_frame(KasinaFrameInput {
            layer_rotation_phases: [0.25; 4],
            respiration: 0.5,
            viewport_points: [100.0, 100.0],
            breath_generation: 0,
            inhaling: false,
        });

        assert!(sanitized.minimum_radius < sanitized.maximum_radius);
        assert_eq!(
            sanitized.inner_rotations_per_second,
            MAX_ROTATIONS_PER_SECOND
        );
        assert_eq!(
            sanitized.expansion_speed_multiplier,
            MAX_EXPANSION_SPEED_MULTIPLIER
        );
        assert_eq!(frame.uniforms.layer_rotation_radians, [0.0; 4]);

        let too_slow = LuminousMandala {
            inner_rotations_per_second: 0.0,
            ..LuminousMandala::default()
        };
        assert_eq!(
            too_slow.sanitized().inner_rotations_per_second,
            MIN_ROTATIONS_PER_SECOND
        );
    }

    #[test]
    fn layer_speeds_are_independent_and_follow_breath_expansion() {
        let visual = LuminousMandala {
            inner_rotations_per_second: 0.10,
            middle_rotations_per_second: 0.20,
            third_rotations_per_second: 0.30,
            gold_rotations_per_second: 0.40,
            expansion_speed_multiplier: 3.0,
            ..LuminousMandala::default()
        };

        assert_array_close(visual.layer_speeds(0.0), [0.10, 0.20, 0.30, 0.40]);
        assert_array_close(visual.layer_speeds(0.5), [0.20, 0.40, 0.60, 0.80]);
        assert_array_close(visual.layer_speeds(1.0), [0.30, 0.60, 0.90, 1.20]);

        let disabled = LuminousMandala {
            rotation_enabled: false,
            ..visual
        };
        assert_eq!(disabled.layer_speeds(1.0), [0.0; 4]);
    }

    #[test]
    fn aurora_vortex_prepares_distinct_effect_parameters_and_speeds() {
        let visual = AuroraVortex::default();
        let frame = visual.prepare_frame(KasinaFrameInput {
            layer_rotation_phases: [0.10, 0.20, 0.30, 0.40],
            respiration: 0.75,
            viewport_points: [1_200.0, 800.0],
            breath_generation: 0,
            inhaling: false,
        });

        assert_eq!(frame.uniforms.style, AURORA_VORTEX_STYLE);
        assert_eq!(frame.uniforms.radius_range, [0.34, 0.88]);
        assert_eq!(frame.uniforms.effect_params, [9.0, 7.5, 1.25, 0.54]);
        assert_eq!(frame.upload_bytes().len(), 64);
        let speeds = visual.layer_speeds(0.0);
        assert!(speeds[0] > speeds[1]);
        assert!(speeds[3] > speeds[2]);

        let invalid = AuroraVortex {
            arms: 100,
            twist: -4.0,
            glow: 8.0,
            hue: 2.25,
            ..visual
        }
        .sanitized();
        assert_eq!(invalid.arms, 24);
        assert_eq!(invalid.twist, 1.0);
        assert_eq!(invalid.glow, 2.5);
        assert_eq!(invalid.hue, 0.25);
    }

    #[test]
    fn organic_kaleidoscope_prepares_breath_generation_and_field_parameters() {
        let visual = OrganicKaleidoscope::default();
        let frame = visual.prepare_frame(KasinaFrameInput {
            layer_rotation_phases: [0.10, 0.20, 0.30, 0.40],
            respiration: 0.75,
            viewport_points: [1_200.0, 800.0],
            breath_generation: 7,
            inhaling: true,
        });

        assert_eq!(frame.uniforms.style, ORGANIC_KALEIDOSCOPE_STYLE);
        assert_eq!(frame.uniforms.radius_range, [0.028, 0.19]);
        assert_eq!(frame.uniforms.effect_params, [18.0, 6.5, 0.82, 0.06]);
        assert!((frame.uniforms.time_seconds - 0.3675).abs() < 1.0e-5);
        assert_eq!(frame.uniforms.instance_count, 7);
        assert_eq!(frame.instance_count(), 1);
        assert_eq!(frame.upload_bytes().len(), 64);
        let contracted_speeds = visual.layer_speeds(0.0);
        let expanded_speeds = visual.layer_speeds(1.0);
        assert!(
            contracted_speeds
                .into_iter()
                .zip(expanded_speeds)
                .all(|(contracted, expanded)| expanded > contracted)
        );

        let invalid = OrganicKaleidoscope {
            seed_radius: 0.50,
            completed_layer_width: 0.01,
            sectors: 100,
            ring_density: -4.0,
            warp: 8.0,
            hue: 2.25,
            ..visual
        }
        .sanitized();
        assert!(invalid.seed_radius < invalid.completed_layer_width);
        assert_eq!(invalid.sectors, 32);
        assert_eq!(invalid.ring_density, 2.0);
        assert_eq!(invalid.warp, 1.5);
        assert_eq!(invalid.hue, 0.25);
    }

    fn assert_array_close(actual: [f32; 4], expected: [f32; 4]) {
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert!((actual - expected).abs() < 1.0e-6);
        }
    }
}
