struct VisualUniforms {
    time_seconds: f32,
    respiration: f32,
    instance_count: u32,
    style: u32,
    viewport_points: vec2<f32>,
    radius_range: vec2<f32>,
    layer_rotation_radians: vec4<f32>,
    effect_params: vec4<f32>,
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

    if visual.style != 0u {
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

fn spectral_color(hue: f32) -> vec3<f32> {
    let shifted = fract(vec3<f32>(hue) + vec3<f32>(0.0, 0.6666667, 0.3333333));
    let rgb = clamp(
        abs(shifted * 6.0 - vec3<f32>(3.0)) - vec3<f32>(1.0),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    return rgb * rgb * (vec3<f32>(3.0) - 2.0 * rgb);
}

fn breath_mandala(local: vec2<f32>) -> vec4<f32> {
    let aspect = max(visual.viewport_points.x / max(visual.viewport_points.y, 1.0), 0.25);
    let point = local * vec2<f32>(aspect, 1.0);
    let screen_radius = length(point);
    let breath = smoothstep(0.0, 1.0, visual.respiration);
    let mandala_radius = mix(visual.radius_range.x, visual.radius_range.y, breath);
    let p = point / mandala_radius;
    let radius = length(p);
    let angle = atan2(p.y, p.x);
    let rotation = visual.layer_rotation_radians;
    // Apply rotation before each symmetry multiplier. This keeps every lace shape at
    // the same physical angular velocity regardless of its number of lobes, while the
    // alternating signs preserve the mandala's counter-rotating layers.
    let inner_angle = angle - rotation.x;
    let middle_angle = angle + rotation.y;
    let third_angle = angle - rotation.z;
    // The outer gold ring is a separate counter-rotating layer. Its circular boundary
    // is rotationally invariant, while its beads make the reverse motion visible.
    let gold_angle = angle + rotation.w;

    let deep_navy = vec3<f32>(0.004, 0.008, 0.028);
    let background_halo = exp(-screen_radius * 2.25) * (0.10 + breath * 0.08);
    var color = deep_navy + vec3<f32>(0.018, 0.035, 0.085) * background_halo;

    let inner_shape = 0.305 + 0.055 * cos(inner_angle * 8.0);
    let inner_lace = line_glow(abs(radius - inner_shape), 0.010, 0.045);
    let middle_shape = 0.545 + 0.115 * cos(middle_angle * 12.0);
    let middle_lace = line_glow(abs(radius - middle_shape), 0.010, 0.048);
    let outer_shape = 0.790 + 0.060 * cos(third_angle * 24.0);
    let outer_lace = line_glow(abs(radius - outer_shape), 0.008, 0.038);

    let spokes = pow(abs(cos(middle_angle * 12.0)), 18.0)
        * smoothstep(0.16, 0.30, radius)
        * (1.0 - smoothstep(0.72, 0.91, radius));
    let bead_wave = abs(sin(gold_angle * 24.0 + radius * 6.0));
    let beads = pow(1.0 - bead_wave, 12.0)
        * line_glow(abs(radius - 0.925), 0.010, 0.035);
    let boundary = line_glow(abs(radius - 1.0), 0.008, 0.045);
    let center = exp(-radius * radius * 30.0);

    let cyan = vec3<f32>(0.10, 0.82, 0.92);
    let violet = vec3<f32>(0.50, 0.20, 0.92);
    let magenta = vec3<f32>(0.94, 0.18, 0.58);
    let gold = vec3<f32>(1.00, 0.66, 0.20);
    color += cyan * inner_lace * 0.86;
    color += mix(violet, magenta, 0.5 + 0.5 * cos(middle_angle * 6.0)) * middle_lace * 0.92;
    color += mix(cyan, violet, 0.5 + 0.5 * sin(third_angle * 8.0)) * outer_lace * 0.82;
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

fn aurora_vortex(local: vec2<f32>) -> vec4<f32> {
    let aspect = max(visual.viewport_points.x / max(visual.viewport_points.y, 1.0), 0.25);
    let point = local * vec2<f32>(aspect, 1.0);
    let screen_radius = length(point);
    let breath = smoothstep(0.0, 1.0, visual.respiration);
    let vortex_radius = mix(visual.radius_range.x, visual.radius_range.y, breath);
    let p = point / vortex_radius;
    let radius = length(p);
    let angle = atan2(p.y, p.x);
    let safe_radius = max(radius, 0.025);
    let log_radius = log(safe_radius);
    let rotation = visual.layer_rotation_radians;
    let arms = max(visual.effect_params.x, 3.0);
    let twist = visual.effect_params.y;
    let glow = visual.effect_params.z;
    let hue = visual.effect_params.w;

    let iris_angle = angle - rotation.x;
    let filament_angle = angle + rotation.y;
    let halo_angle = angle - rotation.z;
    let spark_angle = angle + rotation.w;

    let deep_space = vec3<f32>(0.002, 0.004, 0.018);
    let atmospheric_halo = exp(-screen_radius * 2.1) * (0.08 + breath * 0.08);
    var color = deep_space
        + spectral_color(hue + 0.58) * atmospheric_halo * 0.075;

    let iris_shape = 0.225 + breath * 0.055 + 0.080 * cos(iris_angle * arms);
    let iris = line_glow(abs(radius - iris_shape), 0.009, 0.055);

    let spiral_window = smoothstep(0.18, 0.30, radius)
        * (1.0 - smoothstep(0.73, 1.03, radius));
    let primary_phase = iris_angle * arms + log_radius * twist;
    let opposing_phase = filament_angle * (arms + 3.0) - log_radius * twist * 0.72;
    let primary_filaments = pow(max(1.0 - abs(sin(primary_phase)), 0.0), 13.0)
        * spiral_window;
    let opposing_filaments = pow(max(1.0 - abs(sin(opposing_phase)), 0.0), 12.0)
        * spiral_window;
    let interference = pow(
        max(cos(primary_phase) * cos(opposing_phase), 0.0),
        7.0,
    ) * smoothstep(0.25, 0.38, radius)
        * (1.0 - smoothstep(0.68, 0.92, radius));

    let halo_shape = 0.825
        + 0.034 * sin(halo_angle * arms * 2.0 + log_radius * 1.7);
    let halo = line_glow(abs(radius - halo_shape), 0.008, 0.050);
    let spark_wave = abs(sin(spark_angle * arms * 2.0 + radius * 4.0));
    let sparks = pow(max(1.0 - spark_wave, 0.0), 24.0)
        * line_glow(abs(radius - 0.955), 0.008, 0.036);

    let angular_hue = angle / 6.2831853;
    let primary_color = spectral_color(hue + angular_hue + breath * 0.08);
    let opposing_color = spectral_color(hue + 0.34 - angular_hue * 0.65);
    let iris_color = spectral_color(hue + 0.72 + breath * 0.10);
    let halo_color = spectral_color(hue + 0.16 + radius * 0.22);
    color += iris_color * iris * glow * 0.82;
    color += primary_color * primary_filaments * glow * 0.78;
    color += opposing_color * opposing_filaments * glow * 0.72;
    color += mix(primary_color, opposing_color, 0.5) * interference * glow * 1.35;
    color += halo_color * halo * glow * 0.70;
    color += mix(halo_color, vec3<f32>(1.0, 0.82, 0.42), 0.58) * sparks * glow;

    let corona = exp(-pow(abs(radius - 0.145) * 10.0, 2.0));
    let central_star = exp(-radius * radius * 180.0);
    color += spectral_color(hue + 0.83) * corona * glow * 0.65;
    color += vec3<f32>(0.82, 0.96, 1.0) * central_star * (0.75 + breath * 0.55);
    color *= 0.24 + 0.76 * smoothstep(0.055, 0.19, radius) + central_star;

    let aura = exp(-abs(screen_radius - vortex_radius * 0.82) * 13.0) * 0.11;
    color += spectral_color(hue + breath * 0.12) * aura * glow;
    let vignette = 1.0 - smoothstep(0.72, 1.55, screen_radius);
    color *= 0.50 + 0.50 * vignette;
    return vec4<f32>(color, 1.0);
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    if visual.style == 1u {
        return breath_mandala(input.local);
    }
    if visual.style == 2u {
        return aurora_vortex(input.local);
    }

    let distance_from_center = length(input.local);
    let alpha = 1.0 - smoothstep(0.45, 1.0, distance_from_center);
    let glow = input.color * (1.1 - distance_from_center * 0.35);
    return vec4<f32>(glow * alpha, alpha);
}
