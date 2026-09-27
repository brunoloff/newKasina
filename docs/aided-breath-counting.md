# Aided breath counting

Open **Aided breath counting**, choose 1–4 companions and a session duration,
then **Start together**. Allow microphone access if the operating system asks.
Speak an English number at the end of your own outbreath. Everyone shares one
counter: 1 through 10, then 1 again. Breathe at your own comfortable pace.

The companions have separate breathing rhythms; their turns are not round-robin.
Each has a different generated voice and a cycle-length multiplier. The breathing
pattern has three editable points (start, halfway, settled), joined by smooth
curves over a configurable settling time. Small cycle-to-cycle variations keep
them from becoming a metronome. Their rings show their own rhythms.

The circle matrix keeps the current round at the top and up to nine completed rounds
below it. Each count takes its speaker's color (your counts are rose; companions
match their rings). Ten moves the completed row down and clears the top row;
the oldest row drops away once the matrix is full. Hover over a circle to see its
number and everyone who counted it. Unheard numbers stay neutral if recognition skips ahead.
Resetting clears only the current row; adding time preserves the history, while
starting a new session clears it.

**Allow shared counts** is off by default. With it off, only the first person in
a turn is registered; competing counts are discarded, and spoken numbers must
follow the current 1–10 sequence. Microphone counting pauses during companion
playback and for half a second afterward to avoid recognizing their voices as
yours. Wait until their voice finishes before adding your next count.

Enable **Allow shared counts** before starting a session to get equal colored
slices for people counting together. **Headphones are required** for this mode;
the yellow warning remains next to its checkbox. Listening stays open during
companion playback. This mode uses headphone audio handling, overriding the
speaker echo-cancellation setting. Each person gets one slice, and a shared count
advances only once. Companions finishing within a quarter-second count together.
Your voice can join the same number within 0.45 seconds of its start, even if
recognition finishes after its row moves down. “You” represents one microphone
user; this does not identify multiple real people sharing a microphone.

**I counted** or **Space** adds the next number manually. Space works while the
counting tab is open. **Reset to 1** means the next count will be one. In shared mode a recognized
number can also repair the counter if a previous count was missed.

At the deadline, the companions stop, a bell rings once, and the microphone
closes. **+ X min together** adds the configured extra time; it works during a
session or after its timer finishes. Resuming preserves the counter and the
settling curve's elapsed time. **End session** returns to setup. A running session
continues on other tabs or with the window minimized; the top bar includes Stop.
Closing the app stops all counting audio and microphone capture.

## Speakers and microphone

Speaker mode is enabled by default. The app sends its actual output signal to
local acoustic echo cancellation before detecting and recognizing your speech.
Companions wait while speech is detected or recognition is pending. Shared
counting must be used with headphones: echo cancellation alone did not reliably
separate the user's voice from companion playback on speakers. Keep shared
counting off when using speakers so playback cannot become a microphone count.

The default microphone and output selected in system sound settings are opened
when you start. To change devices, end the session, change system settings, and
start again. In **Sound & microphone**, lower the speech threshold for a quiet
voice, or raise it if room noise keeps the companions waiting. Disable speaker
echo cancellation when using headphones. The volume control includes the bell.

On macOS, microphone permission is under System Settings → Privacy & Security →
Microphone. On Windows, enable microphone access for desktop apps in Settings →
Privacy & security → Microphone. No API key or speech service account is needed.

## Offline model and development

Microphone audio stays in memory on your computer; it is not saved or uploaded.
Recognition uses a pinned 32 MB English Whisper model. Desktop packages include
it, and macOS carries it inside the `.app`. Source builds can fetch it from the
panel's **Download speech recognition** button, or during development:

```sh
python scripts/fetch-speech-model.py
scripts/cargo-local run -p kasina-app
```

The one-time download goes to Hugging Face; SHA-256 and size are checked before
loading. The model manifest and licensing notice are under
`crates/kasina-counting/assets`. Building the speech engine requires CMake and a
C++ compiler and libclang (LLVM). Native bindings must be generated for the host
platform; the speech dependency’s pre-generated Linux bindings cannot be used
on Windows. No Python, eSpeak, or external speech executable is required to run
the packaged app. The generated companion recordings are compiled into the app.

The reusable `kasina-counting` crate owns the session engine, audio capture and
playback, echo cancellation, utterance detection, model loading, and recognition.
It has no dependency on egui or the measurement service. Microphone capture starts
only on an explicit Start or resume action, and the measurement service keeps its
existing hardware ownership and Linux tray workflow.

Automated checks do not open sound devices:

```sh
scripts/cargo-local test -p kasina-counting
scripts/cargo-local run -p kasina-counting --example recognize-fixtures -- \
  target/speech-models/ggml-tiny.en-q5_1.bin
# After downloading the model, test overlapping voices and pure echo offline:
scripts/cargo-local test -p kasina-counting \
  recognizes_same_number_during_simulated_speaker_playback -- --ignored
```

The tests cover timing, wrapping, independent rhythms, expiry and resumption,
stale/echo counts, signal resampling, voice detection, synthetic echo suppression,
and offline recognition of the bundled clips. They do not replace a real-room
microphone test, nor measure accuracy across human accents or overlapping voices.
