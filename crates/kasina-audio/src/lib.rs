//! Short, interruptible feedback cues. Device access and decoding stay off the UI thread.
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Original pyThoughtstream feedback sounds, embedded in the executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackCue {
    Up,
    Down,
    Warning,
}
impl FeedbackCue {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Up => include_bytes!("../assets/thoughtstream/up.wav"),
            Self::Down => include_bytes!("../assets/thoughtstream/down.wav"),
            Self::Warning => include_bytes!("../assets/thoughtstream/warn.wav"),
        }
    }
}

/// Reserved command model for future heartbeat feedback.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCommand {
    Heartbeat {
        frequency_hz: f32,
        duration: Duration,
        volume: f32,
    },
    StopAll,
}

#[derive(Debug)]
enum Command {
    Play(FeedbackCue, f32, Instant, u64),
    Stop,
}

/// Owns a lazy audio worker; dropping the player stops playback and releases the device.
#[derive(Debug)]
pub struct FeedbackPlayer {
    sender: mpsc::Sender<Command>,
    generation: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
}
impl Default for FeedbackPlayer {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let error = Arc::new(Mutex::new(None));
        let worker_generation = Arc::clone(&generation);
        let worker_error = Arc::clone(&error);
        let result = std::thread::Builder::new().name("thoughtstream-audio".into()).spawn(move || {
            let mut stream: Option<rodio::OutputStream> = None;
            let mut sink: Option<rodio::Sink> = None;
            while let Ok(mut command) = receiver.recv() {
                // Replace queued cues, never replay a backlog after device initialization.
                while let Ok(newer) = receiver.try_recv() { command = newer; }
                sink.take();
                if let Command::Play(cue, volume, requested, serial) = command {
                    if stream.is_none() {
                        let callback_error = Arc::clone(&worker_error);
                        let output = rodio::OutputStreamBuilder::from_default_device().and_then(|builder| {
                            builder.with_error_callback(move |failure| {
                                *callback_error.lock().unwrap() = Some(format!("Audio output interrupted: {failure}. Toggle sound to retry."));
                            }).open_stream_or_fallback()
                        });
                        match output {
                            Ok(mut output) => { output.log_on_drop(false); stream = Some(output); }
                            Err(failure) => {
                                *worker_error.lock().unwrap() = Some(format!("Audio unavailable: {failure}. Check your output device, then toggle sound to retry."));
                                continue;
                            }
                        }
                    }
                    if worker_generation.load(Ordering::Acquire) != serial || requested.elapsed() > Duration::from_millis(750) { continue; }
                    match rodio::Decoder::try_from(Cursor::new(cue.bytes())) {
                        Ok(source) => {
                            let playing = rodio::Sink::connect_new(stream.as_ref().unwrap().mixer());
                            playing.set_volume(volume.clamp(0.0, 1.0));
                            playing.append(source);
                            sink = Some(playing);
                            *worker_error.lock().unwrap() = None;
                        }
                        Err(failure) => *worker_error.lock().unwrap() = Some(format!("Could not decode feedback sound: {failure}")),
                    }
                } else {
                    stream.take();
                }
            }
        });
        if let Err(failure) = result {
            *error.lock().unwrap() = Some(format!("Could not start audio: {failure}"));
        }
        Self {
            sender,
            generation,
            error,
        }
    }
}
impl FeedbackPlayer {
    pub fn play(&self, cue: FeedbackCue, volume: f32) {
        let serial = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self
            .sender
            .send(Command::Play(cue, volume, Instant::now(), serial));
    }
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.sender.send(Command::Stop);
    }
    pub fn clear_error(&self) {
        *self.error.lock().unwrap() = None;
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}
impl Drop for FeedbackPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source;
    #[test]
    fn original_cues_decode_with_expected_durations() {
        for (cue, duration) in [
            (FeedbackCue::Up, 0.1),
            (FeedbackCue::Down, 0.1),
            (FeedbackCue::Warning, 0.3),
        ] {
            let source = rodio::Decoder::try_from(Cursor::new(cue.bytes())).unwrap();
            assert_eq!(source.channels(), 1);
            assert_eq!(source.sample_rate(), 44_100);
            assert!((source.total_duration().unwrap().as_secs_f64() - duration).abs() < 0.001);
            assert!(source.into_iter().any(|sample| sample.abs() > 0.01));
        }
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    /// Opt-in desktop audio check; normal test runs never make sound.
    #[test]
    #[ignore = "plays three sounds on the desktop output"]
    fn plays_original_cues_on_default_output() {
        let player = FeedbackPlayer::default();
        for cue in [FeedbackCue::Up, FeedbackCue::Down, FeedbackCue::Warning] {
            player.play(cue, 0.25);
            std::thread::sleep(Duration::from_secs(1));
            assert_eq!(player.error(), None);
        }
        player.stop();
    }
}
