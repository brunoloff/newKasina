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

fn organic_palette(value: f32) -> vec3<f32> {
    let phase = 6.2831853 * (vec3<f32>(value) + vec3<f32>(0.00, 0.19, 0.43));
    return vec3<f32>(0.50, 0.46, 0.43)
        + vec3<f32>(0.48, 0.44, 0.41) * cos(phase);
}

fn layered_organic_palette(
    inner_hue: f32,
    outward_hue: f32,
    outward_blend: f32,
    offset: f32,
) -> vec3<f32> {
    return mix(
        organic_palette(inner_hue + offset),
        organic_palette(outward_hue + offset),
        outward_blend,
    );
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

fn organic_kaleidoscope(local: vec2<f32>) -> vec4<f32> {
    let aspect = max(visual.viewport_points.x / max(visual.viewport_points.y, 1.0), 0.25);
    let point = local * vec2<f32>(aspect, 1.0);
    let screen_radius = length(point);
    let angle = atan2(point.y, point.x);
    let breath = smoothstep(0.0, 1.0, visual.respiration);
    let rotation = visual.layer_rotation_radians;
    let sectors = max(visual.effect_params.x, 4.0);
    let density = visual.effect_params.y;
    let warp = visual.effect_params.z;
    let hue = visual.effect_params.w;

    // Full-screen kasinas draw one instance, so this style reuses the particle instance
    // field as its exact breath generation. The time scalar packs direction and inhale
    // progress: [0.0, 0.5) is inhale, while [0.5, 1.0) is exhale/settling.
    let breath_generation = f32(visual.instance_count);
    let packed_breath_phase = visual.time_seconds;
    let inhaling = packed_breath_phase < 0.5;
    let insertion_progress = select(
        clamp((packed_breath_phase - 0.50) / 0.49, 0.0, 1.0),
        clamp(packed_breath_phase / 0.49, 0.0, 1.0),
        inhaling,
    );

    // Fold the plane into one mirrored wedge. Every operation below is performed on
    // this folded coordinate, so even the slowly changing organic field preserves exact
    // kaleidoscopic symmetry without textures or per-frame CPU geometry.
    let sector_width = 6.2831853 / sectors;
    let wedge_phase = fract((angle - rotation.x) / sector_width + 0.5) - 0.5;
    let folded_angle = abs(wedge_phase) * sector_width;
    let mirror_axis = folded_angle / max(sector_width * 0.5, 0.001);

    // A new generation grows monotonically from zero area and finishes settling if the
    // inhale ends early. Subtracting its current width before indexing the older bands
    // pushes every previous breath outward; layers beyond the viewport simply disappear.
    // The small mirrored perturbation gives boundaries an organic scallop.
    let seed_radius = visual.radius_range.x;
    let completed_layer_width = visual.radius_range.y;
    let inserted_width = completed_layer_width
        * insertion_progress * insertion_progress * (3.0 - 2.0 * insertion_progress);
    let boundary_warp = sin(
        mirror_axis * 3.1415927
            + screen_radius * density * 2.1
            + rotation.y,
    ) * warp * 0.012 * smoothstep(0.04, 0.35, screen_radius);
    let band_radius = max(screen_radius + boundary_warp, 0.0);
    let is_new_layer = band_radius < inserted_width;
    let older_coordinate = max(band_radius - inserted_width, 0.0) / completed_layer_width;
    let older_layer_offset = floor(older_coordinate) + 1.0;
    let layer_generation = select(
        breath_generation - older_layer_offset,
        breath_generation,
        is_new_layer,
    );
    let layer_fraction = select(
        fract(older_coordinate),
        band_radius / max(inserted_width, 0.0001),
        is_new_layer,
    );

    let source_radius = screen_radius * (1.0 + breath * 0.035);
    let folded = vec2<f32>(cos(folded_angle), sin(folded_angle)) * source_radius;
    let morph_phase = rotation.y;
    // CPU phases wrap once per turn. Every shader use must therefore be periodic at that
    // boundary: a non-periodic scale of the wrapped phase would create a visible hue jump.
    let palette_drift = 0.5 - 0.5 * cos(rotation.z);
    let warp_phase = rotation.w;
    let distortion = vec2<f32>(
        sin(source_radius * density * 1.75 + morph_phase + mirror_axis * 2.2),
        cos(source_radius * density * 1.38 - warp_phase + mirror_axis * 3.1),
    ) * warp * 0.105;
    let q = folded + distortion;

    // Several inexpensive continuous fields create broad mineral-like regions, nested
    // rings, fine contour ridges, and occasional jewel points. Their phases move at
    // different rates, so the same algorithm keeps generating new coherent motifs.
    let radial_phase = source_radius * density * 6.2831853;
    let cross_field = sin((q.x * 2.7 + q.y * 3.4) * density + morph_phase)
        * cos((q.y * 2.1 - q.x * 1.6) * density - warp_phase);
    let flowing_field = sin(
        radial_phase
            + warp * 2.3 * sin(q.x * density * 2.2 + morph_phase),
    ) + 0.58 * cos(
        radial_phase * 1.57 - q.y * density * 3.0 + warp_phase,
    );
    let petal_field = 0.5 + 0.5 * cos(
        mirror_axis * 3.1415927
            + sin(radial_phase * 0.34 + morph_phase) * (1.0 + warp * 0.45),
    );
    let material = 0.5 + 0.5 * sin(
        flowing_field * 1.7 + cross_field * 1.2 + radial_phase * 0.24,
    );
    let mineral = 0.5 + 0.5 * cos(
        flowing_field * 1.15 - cross_field * 1.55 - radial_phase * 0.17,
    );

    // Every breath receives a stable deterministic hue. Material variation and palette
    // drift remain deliberately smaller, so each concentric band stays recognizable as
    // one historical breath even while its internal shapes continue to evolve.
    let layer_hue = fract(
        hue
            + layer_generation * 0.381966
            + (hash(layer_generation * 1.731 + 19.17) - 0.5) * 0.09,
    );
    let outward_generation = layer_generation - 1.0;
    let outward_hue = fract(
        hue
            + outward_generation * 0.381966
            + (hash(outward_generation * 1.731 + 19.17) - 0.5) * 0.09,
    );
    let outward_blend = smoothstep(0.88, 1.0, layer_fraction);
    let base = layered_organic_palette(
        layer_hue,
        outward_hue,
        outward_blend,
        material * 0.10 + palette_drift * 0.045,
    );
    let accent = layered_organic_palette(
        layer_hue,
        outward_hue,
        outward_blend,
        0.22 - mineral * 0.09 - palette_drift * 0.032,
    );
    let shadow = layered_organic_palette(
        layer_hue,
        outward_hue,
        outward_blend,
        0.48 + petal_field * 0.07 + palette_drift * 0.025,
    );
    var color = mix(base, accent, smoothstep(0.30, 0.76, mineral));
    color = mix(color, shadow * 0.52, smoothstep(0.58, 0.94, petal_field) * 0.58);
    color *= 0.42 + material * 0.72;
    let stable_breath_tint = layered_organic_palette(
        layer_hue,
        outward_hue,
        outward_blend,
        0.0,
    );
    let material_luminance = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
    color = mix(
        color,
        stable_breath_tint * (0.32 + material_luminance * 1.05),
        0.52,
    );

    let ridge_phase = abs(sin(
        flowing_field * 2.05 + cross_field * 0.75 + radial_phase * 0.42,
    ));
    let ridges = 1.0 - smoothstep(0.025, 0.14, ridge_phase);
    let ring_relief = pow(max(1.0 - abs(sin(
        radial_phase * 0.51 + petal_field * 2.4 - morph_phase,
    )), 0.0), 5.0);
    let jewels = pow(max(cos(
        radial_phase * 0.72 + mirror_axis * 6.2831853 + warp_phase,
    ), 0.0), 18.0) * smoothstep(0.22, 0.82, mineral);
    color += layered_organic_palette(
        layer_hue,
        outward_hue,
        outward_blend,
        0.10 + palette_drift * 0.04,
    ) * ridges * 0.44;
    color += layered_organic_palette(
        layer_hue,
        outward_hue,
        outward_blend,
        0.37 - palette_drift * 0.03,
    ) * ring_relief * 0.20;
    color += vec3<f32>(0.90, 0.96, 0.78) * jewels * 0.52;

    let boundary_distance = min(layer_fraction, 1.0 - layer_fraction)
        * completed_layer_width;
    let layer_seam = 1.0 - smoothstep(0.003, 0.022, boundary_distance);
    color = mix(
        color * 0.72,
        layered_organic_palette(layer_hue, outward_hue, outward_blend, 0.16) * 0.92,
        layer_seam * 0.52,
    );

    // The newborn glint fades to zero before its layer is committed. Therefore the
    // generation-N, progress-1 frame and generation-(N+1), progress-0 frame evaluate to
    // the same image; the next color then grows continuously out of a zero-area center.
    let newest_mask = select(0.0, 1.0, is_new_layer);
    let core_color = layered_organic_palette(
        layer_hue,
        outward_hue,
        outward_blend,
        0.055 + palette_drift * 0.03,
    );
    let birth_visibility = smoothstep(0.0, 0.08, insertion_progress);
    let newborn_glint = exp(
        -screen_radius * screen_radius
            / max(seed_radius * seed_radius * birth_visibility * 0.32, 0.00002),
    );
    color += core_color * newborn_glint * newest_mask * birth_visibility
        * (1.0 - insertion_progress) * 0.60;

    let outer_vignette = 1.0 - smoothstep(1.32, 1.82, screen_radius);
    color *= outer_vignette;
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
    if visual.style == 3u {
        return organic_kaleidoscope(input.local);
    }

    let distance_from_center = length(input.local);
    let alpha = 1.0 - smoothstep(0.45, 1.0, distance_from_center);
    let glow = input.color * (1.1 - distance_from_center * 0.35);
    return vec4<f32>(glow * alpha, alpha);
}
