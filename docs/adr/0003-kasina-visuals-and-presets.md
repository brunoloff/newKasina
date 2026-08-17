# ADR 0003: Kasina visuals and presets

Date: 2026-08-13

## Status

Accepted and implemented.

## Context

A breath kasina is not one fixed screen. The application needs multiple visual
implementations, multiple reusable configurations of each implementation, and a way to
add new shapes without coupling the UI, respiration normalization, and wgpu callback.

Rust traits are the appropriate equivalent of the requested implementation class, while
an enum is better for durable serialized data: traits provide runtime behavior, and an
explicit tagged enum makes every persisted implementation and migration exhaustive.

## Decision

`kasina-render` exposes the `KasinaVisual` trait. An implementation owns typed options and
reduces a common `KasinaFrameInput`—elapsed time, normalized breathing expansion, and
viewport size—to the retained renderer's fixed-size `PreparedVisualFrame`. The renderer
does not inspect implementation-specific application settings.

`LuminousMandala` is the first implementation. Its options are minimum radius, maximum
radius, rotation enabled, and rotation speed in complete rotations per second. Its three
lace shapes apply their alternating rotation directions before their symmetry multipliers,
so their differing lobe counts do not alter their visible speed. The outer gold ring and
beads counter-rotate relative to the third lace shape. Each of the four layers has its own
contracted speed in the selectable range of 0.01 to 10 rotations per second. A shared
full-expansion multiplier from 1 to 10 linearly raises every layer's instantaneous speed
with the smoothed breath expansion; effective speeds saturate at 10 rotations per second.
The app integrates phase from frame deltas so changing respiration or settings changes
velocity without jumping angle. It sanitizes values loaded from disk before preparing the
64-byte uniform update containing four independently integrated layer phases and four
implementation-specific effect parameters.

`AuroraVortex` is the second implementation. It combines two opposing logarithmic
filament fields, a breathing iris, an independently rotating halo, and an orbiting spark
ring. Presets control radius range, four layer speeds, breath-speed multiplication, spiral
arm count, twist, glow, and spectral hue. The same four-channel phase clock drives every
implementation without putting transient animation state into persisted presets.

`OrganicKaleidoscope` is the third implementation. Its fragment shader folds a procedural
radial material field into mirrored angular wedges, then combines independently evolving
flow, petal, contour, relief, and palette phases. It does not need an image texture or
per-frame geometry. A hysteretic breath-direction classifier advances one generation only
on a confirmed exhale-to-inhale transition. The new generation starts as a distinct color
seed, grows monotonically into a completed band during inhale, and pushes every older band
outward. Exhale keeps the spatial insertion committed while the radial material contracts
continuously; layers beyond the viewport are discarded implicitly by clipping. Band colors
are deterministic functions of their generation, so the visible rings form a rolling breath
history without uploading a color array. The state representation makes a completed
generation `N + 1.0` position-equivalent to the next generation's `N+1 + 0.0`; a new seed
therefore begins at zero area rather than replacing a finite disk. CPU insertion progress is
monotonic, finishes smoothly after the inhale peak, and uses a zero-velocity smoothstep in
the shader. Procedural phase is continuous across radial seams, every wrapped animation
phase is genuinely periodic, and adjacent generation palettes cross-fade before the
discrete band index changes. Presets expose seed size,
completed layer width,
sector count, ring density, organic warp, palette hue, four animation-cycle rates, and a
breath-speed multiplier. Generation, direction, and insertion progress are packed into an
otherwise unused kasina instance field plus one scalar while retaining the shared
fixed-size uniform and a single full-screen draw instance.

`PaperDisk` is the fourth implementation. It renders a quiet off-white paper circle over
a warm wooden tabletop and maps only the circle radius to smoothed breath expansion. Both
material fields are fixed in screen-point space, so breathing reveals and conceals the
paper without stretching, sliding, or rescaling its fibers. A small set of band-limited
procedural noise layers bends
the broad grain, adds fine pores and paper mottling, and curls the grain around two
elongated knots. Analytic derivative-smoothed edges, a faint inset rim, and a slightly
offset contact shadow with a broad penumbra make the disk read as paper resting above the
wood. No image asset, animation clock, extra geometry, or additional draw call is needed.
Presets expose radius range, wood-grain scale and contrast, paper-texture strength, and
shadow strength through the existing four effect parameters.

The app stores `KasinaPreset` values. Each preset has a stable numeric ID, editable name,
and a tagged `KasinaVisualPreset` enum containing that implementation's typed options.
The active preset is independent of the preset being edited. Adding a preset duplicates
the edited preset as a starting point; any preset can be removed while at least one
remains.

Application settings use a versioned JSON document in the platform configuration
directory. Settings writes are coalesced and performed by a dedicated background thread,
then installed through a temporary-file replacement. The render and UI threads never
write the file. Invalid option ranges are repaired on load; a settings schema newer than
the running app produces a visible fallback warning instead of being partially applied.

Settings also store tab visibility. Breath kasina is the only optional tab enabled by
default. Settings is deliberately not represented by a visibility flag and is always
added to navigation, so a user cannot hide the only route back to configuration.

## Adding another visual

1. Add a typed options structure and `KasinaVisual` implementation in `kasina-render`.
2. Add the corresponding shader/pipeline style while retaining resources across frames.
3. Add a tagged `KasinaVisualPreset` variant in the app.
4. Add that variant's controls to the exhaustive Settings editor match.
5. Supply at least one default preset and tests for option sanitization, serialization,
   shader parsing, and constant-size frame preparation.

## Consequences

Adding an implementation requires explicit renderer, persistence, and editor work, which
is intentional: unknown visual data is never silently misinterpreted. Presets remain
small, human-readable, versionable, and independent of live sensor state. The common
breathing normalizer can drive every implementation consistently.
