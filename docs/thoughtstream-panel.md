# ThoughtStream panel

Open **ThoughtStream** in the app's sidebar. It appears by default; its visibility
can also be changed in Settings. Acquisition and recording remain in the sensor
service. This panel reads the same resistance stream as Raw Signals.

The display is a borderless two-by-two layout: a large signed **change in
resistance** on the upper left and an equally large **skin resistance** reading on
the upper right, with their respective graphs directly below. Both values are in
kΩ. Delta compares consecutive displayed averages and uses two decimal places;
absolute resistance uses one decimal place. They share the same font size.

The delta trace covers 60 seconds and shades increases green and decreases red.
The absolute trace covers three minutes. Hover over either graph for a numeric
reading and its age. Gaps break traces, and delta needs two fresh averages after a
gap. Audio feedback retains its separate reference and timing rules.

Each graph calculates its 20th and 80th percentiles from finite values within its
visible time window, then expands those bounds to include **all readings from the
latest 10 seconds**, including points exactly 10 seconds old. Percentiles use
linear interpolation. Delta segments and their shading are clipped at the graph
boundaries, so out-of-range values do not leave empty columns or expand the scale. Its fill uses
one vertical colour gradient across the whole plot. A retained predecessor lets the
trace reach the left edge; genuine gaps in data still break it. The absolute graph
continues to omit older out-of-range points and break its line. Current
numbers, sound feedback, retained samples and recordings are unaffected. Empty or
flat histories get a small nonzero display range. Delta's zero line appears when
zero is within its range; bounds need not be symmetric.

**Sound off / Sound on** and **Space** toggle feedback. Holding Space does not keep
toggling; typing in a numeric setting does not toggle sound. Sound starts muted and
is switched off when leaving the panel. Closing the app stops audio. Missing, stale
or invalid readings pause feedback; a fresh baseline is established after gaps,
recalibration, source changes, unmuting or changing settings. This avoids treating
old history or reconnections as a sudden resistance drop. A low-battery notice is
shown separately. Audio errors appear in the panel; toggle sound to retry after
fixing the desktop output device.

## Timing and sound

Expand **Timing & feedback settings** to adjust saved preferences:

| Control | Default | Effect |
| --- | --- | --- |
| Average / display interval | 1 s | Average newly received valid readings and evaluate cues at this interval |
| Base cue interval | 1 s | Minimum regular-cue spacing below the slowdown threshold |
| Slowdown starts at | 100 kΩ | At/above this value, spacing is base × (1 + resistance / threshold) |
| Maximum cue interval | 7 s | Caps the resistance-dependent spacing; actual cues occur at the next display update |
| Warning drop | More than 4 kΩ | Plays the warning immediately at the next averaged reading, bypassing the regular timer |
| Volume | 50% | Level of all three sounds |

**Restore Python timing defaults** restores the timing and warning values while
keeping volume. Preferences are stored with the app's other settings. Muted/on
state is intentionally not restored. Tone preview buttons are available when sound
is enabled.

The rules reproduce the accessible `tts2` feedback mode in Bruno's
`pyThoughtstream3.py`. Rise/fall compares with the last audio reference, not every
raw sample. Equal resistance uses the falling tone. A warning updates the reference
but does not reset the regular-cue timer. The first average establishes a quiet
baseline; the original's artificial zero reference is not reproduced. The older,
unexposed speech mode is not used.

The original `snd/up.wav`, `snd/down.wav` and `snd/warn.wav` are embedded in
`kasina-audio`; installation does not need the Python project, pygame, or sound
files in the working directory. A dedicated worker plays short, interruptible cues
through the desktop's default output using [Rodio](https://docs.rs/rodio/0.21.1/rodio/).
New cues replace the current cue. Muting cancels pending and playing feedback.
