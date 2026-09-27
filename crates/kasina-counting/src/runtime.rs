use crate::{
    audio::{Audio, Frame},
    dsp::VoiceDetector,
    engine::{Engine, Event, Phase, Settings, Snapshot, Speaker},
    speech::Recognizer,
    voices,
};
use aec3::{nodes::audio::AudioFormat, pipelines::linear};
use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct Status {
    pub snapshot: Snapshot,
    pub loading: bool,
    pub microphone: String,
    pub output: String,
    pub level: f32,
    pub recognizing: bool,
    pub notice: String,
    pub error: Option<String>,
}
impl Default for Status {
    fn default() -> Self {
        Self {
            snapshot: Engine::new(Settings::default()).snapshot(0.0),
            loading: false,
            microphone: String::new(),
            output: String::new(),
            level: 0.0,
            recognizing: false,
            notice: "Ready when you are".into(),
            error: None,
        }
    }
}
#[derive(Debug)]
pub enum Command {
    Start(Settings, PathBuf),
    Stop,
    Extend,
    ManualCount,
    ResetCount,
    Shutdown,
}
#[derive(Debug)]
pub struct Controller {
    sender: Sender<Command>,
    status: Arc<Mutex<Status>>,
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}
impl Controller {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let state = status.clone();
        let cancel = cancelled.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let closing = shutdown.clone();
        let thread = thread::spawn(move || worker(receiver, state, cancel, closing));
        Self {
            sender,
            status,
            cancelled,
            shutdown,
            thread: Some(thread),
        }
    }
    pub fn send(&self, command: Command) {
        if matches!(command, Command::Shutdown) {
            self.shutdown.store(true, Ordering::Relaxed);
        }
        if matches!(command, Command::Stop | Command::Shutdown) {
            self.cancelled.store(true, Ordering::Relaxed);
        }
        if matches!(command, Command::Start(..)) {
            let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
            if status.loading || status.snapshot.phase != Phase::Ready {
                return;
            }
            self.cancelled.store(false, Ordering::Relaxed);
            status.loading = true;
            status.error = None;
            status.notice = "Preparing local speech recognition…".into();
        }
        let _ = self.sender.send(command);
    }
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn worker(
    receiver: Receiver<Command>,
    status: Arc<Mutex<Status>>,
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Shutdown => break,
            Command::Start(mut settings, path) => {
                settings.sanitize();
                let result = session(settings, path, &receiver, &status, cancelled.clone());
                let mut state = status.lock().unwrap_or_else(|p| p.into_inner());
                state.loading = false;
                state.recognizing = false;
                state.level = 0.0;
                state.snapshot.phase = Phase::Ready;
                state.notice = "Ready when you are".into();
                if let Err(error) = result {
                    state.error = Some(format!("{error:#}"));
                    state.notice = "Session stopped".into();
                }
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
            _ => {}
        }
    }
}
// Every session owns its streams. No input opens until Start, and expiry closes input.
fn session(
    settings: Settings,
    path: PathBuf,
    commands: &Receiver<Command>,
    shared: &Arc<Mutex<Status>>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let recognizer = Recognizer::load(&path).context("Load local speech recognition")?;
    if cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut audio = Audio::open().context("Open counting audio")?;
    let format = AudioFormat::ten_ms(16000, 1);
    let mut aec = linear::builder(format, format)
        .initial_delay_ms(60)
        .enable_gain_controller2(false)
        .build()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let (jobs, receive_jobs) = mpsc::sync_channel::<(u64, f64, Vec<f32>)>(1);
    let (results, receive_results) = mpsc::channel();
    let cancel = cancelled.clone();
    let inference = thread::spawn(move || {
        while let Ok((generation, started, samples)) = receive_jobs.recv() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let result = recognizer.recognize(&samples, cancel.clone());
            let _ = results.send((generation, started, result));
        }
    });
    let origin = Instant::now();
    let mut engine = Engine::new(settings.clone());
    engine.start(0.0);
    let mut detector = VoiceDetector::new(settings.microphone_threshold);
    let mut in_flight = false;
    let mut generation = 0_u64;
    let mut recognition_floor = 0.0;
    let mut quiet_until = 0.0;
    {
        let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
        state.loading = false;
        state.microphone = audio.microphone.clone();
        state.output = audio.output.clone();
        state.notice = "Listening for your count".into();
    }
    let outcome = (|| -> Result<()> {
        loop {
            let now = origin.elapsed().as_secs_f64();
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            // Deadline is checked before recognition and before any new number is queued.
            if let Some(Event::Bell) = engine.tick(now, true) {
                generation += 1;
                in_flight = false;
                detector = VoiceDetector::new(settings.microphone_threshold);
                audio.close_microphone();
                audio.clear();
                audio.play(&voices::bell(), settings.volume);
                shared.lock().unwrap_or_else(|p| p.into_inner()).notice =
                    "Quiet practice · add time whenever you need company".into();
            }
            while let Ok(command) = commands.try_recv() {
                match command {
                    Command::Stop | Command::Shutdown => {
                        cancelled.store(true, Ordering::Relaxed);
                        return Ok(());
                    }
                    Command::Extend => {
                        if engine.snapshot(now).phase == Phase::Quiet {
                            audio.open_microphone()?;
                            audio.clear();
                            aec.reset_aec3()
                                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                            detector = VoiceDetector::new(settings.microphone_threshold);
                            generation += 1;
                            in_flight = false;
                        }
                        recognition_floor = now;
                        engine.extend(now);
                        shared.lock().unwrap_or_else(|p| p.into_inner()).notice =
                            "Listening for your count".into();
                    }
                    Command::ManualCount => {
                        recognition_floor = now;
                        engine.manual_count(now);
                        generation += 1;
                        in_flight = false;
                    }
                    Command::ResetCount => {
                        recognition_floor = now;
                        engine.reset_count();
                        generation += 1;
                        in_flight = false;
                    }
                    Command::Start(..) => {}
                }
            }
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            if audio.take_overflow() {
                anyhow::bail!(
                    "Audio processing could not keep up. Close busy applications, then start again."
                );
            }
            while let Ok(frame) = audio.frames.try_recv() {
                match frame {
                    Frame::Render(frame) => {
                        if settings.speakers {
                            aec.handle_render_frame(&frame)
                                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                        }
                    }
                    Frame::Capture(frame, captured)
                        if engine.snapshot(now).phase == Phase::Counting =>
                    {
                        if now - captured.saturating_duration_since(origin).as_secs_f64() > 0.3 {
                            continue;
                        }
                        let mut cleaned = frame;
                        if settings.speakers
                            && !aec
                                .process_capture_frame(&frame, &mut cleaned)
                                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                        {
                            continue;
                        }
                        if let Some(utterance) = detector.push(
                            &cleaned,
                            captured.saturating_duration_since(origin).as_secs_f64(),
                        ) && !in_flight
                            && jobs
                                .try_send((
                                    generation,
                                    utterance.started.max(0.0),
                                    utterance.samples,
                                ))
                                .is_ok()
                        {
                            in_flight = true;
                        }
                    }
                    Frame::Capture(..) => {}
                    Frame::Error(error) => {
                        anyhow::bail!("{error}. Check system sound settings, then start again.")
                    }
                }
            }
            while let Ok((result_generation, started, result)) = receive_results.try_recv() {
                if result_generation != generation {
                    continue;
                }
                in_flight = false;
                quiet_until = now + 0.25;
                let result = result.context("Recognize spoken number")?;
                if started < recognition_floor {
                    continue;
                }
                if let Some(number) = result.number {
                    if engine.heard(number, started, now).is_some() {
                        shared.lock().unwrap_or_else(|p| p.into_inner()).notice =
                            format!("Heard you count {number}");
                    }
                } else {
                    shared.lock().unwrap_or_else(|p| p.into_inner()).notice =
                        "Count not clear · say a number again, or press Space".into();
                }
            }
            if let Some(Event::Count {
                number,
                speaker: Speaker::Companion(index),
            }) = engine.tick(
                now,
                audio.busy() || detector.busy() || in_flight || now < quiet_until,
            ) {
                audio.play(&voices::number(index, number)?, settings.volume);
            }
            {
                let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
                state.snapshot = engine.snapshot(now);
                state.level = detector.level;
                state.recognizing = in_flight;
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    })();
    audio.clear();
    audio.close_microphone();
    cancelled.store(true, Ordering::Relaxed);
    drop(jobs);
    let _ = inference.join();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_controller_closes_without_opening_audio() {
        drop(Controller::new());
    }
    #[test]
    fn missing_model_is_reported_without_opening_audio() {
        let controller = Controller::new();
        controller.send(Command::Start(
            Settings::default(),
            PathBuf::from("/missing-newkasina-test-model.bin"),
        ));
        let until = Instant::now() + Duration::from_secs(2);
        while controller.status().loading && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.status().error.is_some());
        assert_eq!(controller.status().snapshot.phase, Phase::Ready);
    }
}
