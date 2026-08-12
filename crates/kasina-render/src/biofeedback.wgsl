struct VisualUniforms {
    time_seconds: f32,
    respiration: f32,
    instance_count: u32,
    padding: u32,
    viewport_points: vec2<f32>,
    padding_2: vec2<f32>,
};

@group(0) @binding(0)
var<uniform> visual: VisualUniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) color: vec3<f32>,
};

fn hash(value: f32) -> f32 {
    return fract(sin(value * 91.3458) * 47453.5453);
}

@vertex
fn vertex_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let id = f32(instance_index);
    let count = max(f32(visual.instance_count), 1.0);
    let angle = id * 2.399963 + visual.time_seconds * (0.035 + hash(id) * 0.04);
    let radius = sqrt((id + 0.5) / count) * (0.78 + visual.respiration * 0.12);
    let aspect = max(visual.viewport_points.x / max(visual.viewport_points.y, 1.0), 0.25);
    let center = vec2<f32>(cos(angle) * radius / aspect, sin(angle) * radius);
    let size = mix(0.003, 0.012, hash(id + 17.0)) * (0.8 + visual.respiration * 0.5);
    let local = corners[vertex_index];
    let pulse = 0.5 + 0.5 * sin(angle * 2.0 - visual.time_seconds * 0.7);

    var output: VertexOutput;
    output.position = vec4<f32>(center + local * size, 0.0, 1.0);
    output.local = local;
    output.color = mix(
        vec3<f32>(0.08, 0.42, 0.60),
        vec3<f32>(0.72, 0.30, 0.46),
        pulse,
    );
    return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let distance_from_center = length(input.local);
    let alpha = 1.0 - smoothstep(0.45, 1.0, distance_from_center);
    let glow = input.color * (1.1 - distance_from_center * 0.35);
    return vec4<f32>(glow * alpha, alpha);
}
