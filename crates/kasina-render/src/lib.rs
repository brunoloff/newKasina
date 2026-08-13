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
    _padding_2: [f32; 2],
}

const PARTICLE_STYLE: u32 = 0;
const BREATH_KASINA_STYLE: u32 = 1;

/// CPU-side input prepared for one biofeedback draw.
///
/// Construction performs all normalization needed before the callback reaches wgpu. The
/// resulting uniform upload is fixed at 32 bytes regardless of the instance count.
#[derive(Debug, Clone, Copy)]
pub struct PreparedVisualFrame {
    uniforms: VisualUniforms,
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
                _padding_2: [0.0; 2],
            },
        }
    }

    /// Prepare the constant-cost analytic breath mandala.
    #[must_use]
    pub fn breath_kasina(time_seconds: f32, respiration: f32, viewport_points: [f32; 2]) -> Self {
        Self {
            uniforms: VisualUniforms {
                time_seconds,
                respiration: respiration.clamp(0.0, 1.0),
                instance_count: 1,
                style: BREATH_KASINA_STYLE,
                viewport_points: [viewport_points[0].max(1.0), viewport_points[1].max(1.0)],
                _padding_2: [0.0; 2],
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
        self.uniforms.instance_count
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
            _padding_2: [0.0; 2],
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
        time_seconds: f32,
        respiration: f32,
    ) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
        let prepared = PreparedVisualFrame::breath_kasina(
            time_seconds,
            respiration,
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
        render_pass.draw(0..6, 0..self.uniforms.instance_count);
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
    fn prepared_frame_clamps_inputs_and_has_a_fixed_upload() {
        let low = PreparedVisualFrame::new(2.0, -4.0, 0, [0.0, -10.0]);
        let high = PreparedVisualFrame::new(2.0, 4.0, 100_000, [1_920.0, 1_080.0]);

        assert_eq!(low.uniforms.respiration, 0.0);
        assert_eq!(low.uniforms.instance_count, 1);
        assert_eq!(low.uniforms.viewport_points, [1.0, 1.0]);
        assert_eq!(high.uniforms.respiration, 1.0);
        assert_eq!(high.uniforms.instance_count, 100_000);
        assert_eq!(low.upload_bytes().len(), 32);
        assert_eq!(high.upload_bytes().len(), low.upload_bytes().len());
    }

    #[test]
    fn breath_kasina_uses_one_full_screen_instance() {
        let frame = PreparedVisualFrame::breath_kasina(3.0, 1.5, [900.0, 600.0]);

        assert_eq!(frame.uniforms.respiration, 1.0);
        assert_eq!(frame.uniforms.style, BREATH_KASINA_STYLE);
        assert_eq!(frame.instance_count(), 1);
        assert_eq!(frame.upload_bytes().len(), 32);
    }
}
