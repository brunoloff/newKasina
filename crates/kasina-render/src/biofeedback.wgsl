struct VisualUniforms {
    time_seconds: f32,
    respiration: f32,
    instance_count: u32,
    style: u32,
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
    let local = corners[vertex_index];
    var output: VertexOutput;

    if visual.style == 1u {
        output.position = vec4<f32>(local, 0.0, 1.0);
        output.local = local;
        output.color = vec3<f32>(0.0);
        return output;
    }

    let id = f32(instance_index);
    let count = max(f32(visual.instance_count), 1.0);
    let angle = id * 2.399963 + visual.time_seconds * (0.035 + hash(id) * 0.04);
    let radius = sqrt((id + 0.5) / count) * (0.78 + visual.respiration * 0.12);
    let aspect = max(visual.viewport_points.x / max(visual.viewport_points.y, 1.0), 0.25);
    let center = vec2<f32>(cos(angle) * radius / aspect, sin(angle) * radius);
    let size = mix(0.003, 0.012, hash(id + 17.0)) * (0.8 + visual.respiration * 0.5);
    let pulse = 0.5 + 0.5 * sin(angle * 2.0 - visual.time_seconds * 0.7);

    output.position = vec4<f32>(center + local * size, 0.0, 1.0);
    output.local = local;
    output.color = mix(
        vec3<f32>(0.08, 0.42, 0.60),
        vec3<f32>(0.72, 0.30, 0.46),
        pulse,
    );
    return output;
}

fn line_glow(distance_to_line: f32, width: f32, glow_width: f32) -> f32 {
    let core = 1.0 - smoothstep(0.0, width, distance_to_line);
    let glow = 1.0 - smoothstep(width, glow_width, distance_to_line);
    return core + glow * 0.38;
}

fn breath_mandala(local: vec2<f32>) -> vec4<f32> {
    let aspect = max(visual.viewport_points.x / max(visual.viewport_points.y, 1.0), 0.25);
    let point = local * vec2<f32>(aspect, 1.0);
    let screen_radius = length(point);
    let breath = smoothstep(0.0, 1.0, visual.respiration);
    let mandala_radius = mix(0.40, 0.82, breath);
    let p = point / mandala_radius;
    let radius = length(p);
    let angle = atan2(p.y, p.x);
    let rotation = visual.time_seconds * 0.055;

    let deep_navy = vec3<f32>(0.004, 0.008, 0.028);
    let background_halo = exp(-screen_radius * 2.25) * (0.10 + breath * 0.08);
    var color = deep_navy + vec3<f32>(0.018, 0.035, 0.085) * background_halo;

    let inner_shape = 0.305 + 0.055 * cos(angle * 8.0 - rotation * 2.0);
    let inner_lace = line_glow(abs(radius - inner_shape), 0.010, 0.045);
    let middle_shape = 0.545 + 0.115 * cos(angle * 12.0 + rotation);
    let middle_lace = line_glow(abs(radius - middle_shape), 0.010, 0.048);
    let outer_shape = 0.790 + 0.060 * cos(angle * 24.0 - rotation * 0.7);
    let outer_lace = line_glow(abs(radius - outer_shape), 0.008, 0.038);

    let spokes = pow(abs(cos(angle * 12.0 + rotation * 0.4)), 18.0)
        * smoothstep(0.16, 0.30, radius)
        * (1.0 - smoothstep(0.72, 0.91, radius));
    let bead_wave = abs(sin(angle * 24.0 - rotation + radius * 6.0));
    let beads = pow(1.0 - bead_wave, 12.0)
        * line_glow(abs(radius - 0.925), 0.010, 0.035);
    let boundary = line_glow(abs(radius - 1.0), 0.008, 0.045);
    let center = exp(-radius * radius * 30.0);

    let cyan = vec3<f32>(0.10, 0.82, 0.92);
    let violet = vec3<f32>(0.50, 0.20, 0.92);
    let magenta = vec3<f32>(0.94, 0.18, 0.58);
    let gold = vec3<f32>(1.00, 0.66, 0.20);
    color += cyan * inner_lace * 0.86;
    color += mix(violet, magenta, 0.5 + 0.5 * cos(angle * 6.0)) * middle_lace * 0.92;
    color += mix(cyan, violet, 0.5 + 0.5 * sin(angle * 8.0)) * outer_lace * 0.82;
    color += magenta * spokes * 0.36;
    color += gold * beads * 0.92;
    color += mix(cyan, gold, breath) * boundary * 0.72;
    color += mix(violet, cyan, breath) * center * 1.45;

    let interior = 1.0 - smoothstep(0.88, 1.06, radius);
    let aura = exp(-abs(screen_radius - mandala_radius) * 18.0) * 0.13;
    color += mix(violet, cyan, breath) * (interior * 0.035 + aura);

    let vignette = 1.0 - smoothstep(0.70, 1.55, screen_radius);
    color *= 0.55 + 0.45 * vignette;
    return vec4<f32>(color, 1.0);
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    if visual.style == 1u {
        return breath_mandala(input.local);
    }

    let distance_from_center = length(input.local);
    let alpha = 1.0 - smoothstep(0.45, 1.0, distance_from_center);
    let glow = input.color * (1.1 - distance_from_center * 0.35);
    return vec4<f32>(glow * alpha, alpha);
}
