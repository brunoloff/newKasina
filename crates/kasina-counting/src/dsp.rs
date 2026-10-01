use std::collections::VecDeque;

/// Streaming linear resampling, with two low-pass stages before downsampling.
/// State spans device callbacks, including non-integral 44.1 kHz / 16 kHz ratios.
#[derive(Debug)]
pub(crate) struct Resampler {
    ratio: f64,
    position: f64,
    previous: Option<f32>,
    filter: [f32; 2],
    alpha: f32,
}
impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let alpha = if from > to {
            1.0 - (-std::f32::consts::TAU * to as f32 * 0.4 / from as f32).exp()
        } else {
            1.0
        };
        Self {
            ratio: from as f64 / to as f64,
            position: 0.0,
            previous: None,
            filter: [0.0; 2],
            alpha,
        }
    }
    pub fn push(&mut self, input: f32, mut emit: impl FnMut(f32)) {
        self.filter[0] += self.alpha * (input - self.filter[0]);
        self.filter[1] += self.alpha * (self.filter[0] - self.filter[1]);
        let sample = self.filter[1];
        if let Some(previous) = self.previous {
            while self.position < 1.0 {
                emit(previous + (sample - previous) * self.position as f32);
                self.position += self.ratio;
            }
            self.position -= 1.0;
        }
        self.previous = Some(sample);
    }
}
pub(crate) fn echo_canceller() -> anyhow::Result<aec3::pipelines::linear::LinearPipeline> {
    use aec3::{nodes::audio::AudioFormat, pipelines::linear};
    let format = AudioFormat::ten_ms(16000, 1);
    linear::builder(format, format)
        .initial_delay_ms(60)
        .enable_gain_controller2(false)
        .export_linear_output(true)
        .build()
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

/// Preserve syllables during double-talk using linear echo-subtracted audio.
/// Retain stronger suppression when there is no independent-voice evidence.
pub(crate) fn capture_frame(
    aec: &mut aec3::pipelines::linear::LinearPipeline,
    frame: &[f32; 160],
    independent: bool,
) -> anyhow::Result<Option<[f32; 160]>> {
    let mut cleaned = [0.0; 160];
    let available = aec
        .process_capture_frame(frame, &mut cleaned)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    // Always drain this sink, including frames where we prefer suppressed audio.
    if let Some(linear) = aec
        .try_pull_linear_output()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        && independent
    {
        cleaned.copy_from_slice(linear.payload().samples());
    }
    Ok(available.then_some(cleaned))
}

/// Conservative extra evidence for joining a companion's count. Compare the
/// microphone waveform to recent actual speaker output across possible delays.
/// A matching echo is not evidence of a second person, even if ASR hears it.
#[derive(Debug, Default)]
pub(crate) struct EchoEvidence {
    render: VecDeque<f32>,
    capture: VecDeque<f32>,
}
impl EchoEvidence {
    pub fn render(&mut self, frame: &[f32; 160]) {
        self.render.extend(
            frame
                .as_chunks::<8>()
                .0
                .iter()
                .map(|chunk| chunk.iter().sum::<f32>() / 8.0),
        );
        while self.render.len() > 1000 {
            self.render.pop_front();
        }
    }
    pub fn independent(&mut self, frame: &[f32; 160]) -> bool {
        self.capture.extend(
            frame
                .as_chunks::<8>()
                .0
                .iter()
                .map(|chunk| chunk.iter().sum::<f32>() / 8.0),
        );
        while self.capture.len() > 160 {
            self.capture.pop_front();
        }
        if self.capture.len() < 160 {
            return false;
        }
        let capture = self.capture.make_contiguous();
        let energy: f32 = capture.iter().map(|v| v * v).sum();
        if energy < 1e-7 {
            return false;
        }
        let render = self.render.make_contiguous();
        for window in render.windows(capture.len()) {
            let mut reference_energy = 0.0;
            let mut cross = 0.0;
            for (reference, input) in window.iter().zip(capture.iter()) {
                reference_energy += reference * reference;
                cross += reference * input;
            }
            if reference_energy > 1e-7 && cross * cross > 0.55 * energy * reference_energy {
                return false;
            }
        }
        true
    }
}

#[derive(Debug)]
pub(crate) struct Utterance {
    pub samples: Vec<f32>,
    pub started: f64,
    pub independent_voice: bool,
}
#[derive(Debug)]
pub(crate) struct VoiceDetector {
    threshold: f32,
    pre_roll: VecDeque<f32>,
    samples: Vec<f32>,
    hot: usize,
    independent_frames: usize,
    quiet: usize,
    started: f64,
    pub level: f32,
}
impl VoiceDetector {
    pub fn new(threshold: f32) -> Self {
        Self {
            threshold,
            pre_roll: VecDeque::with_capacity(3200),
            samples: Vec::new(),
            hot: 0,
            independent_frames: 0,
            quiet: 0,
            started: 0.0,
            level: 0.0,
        }
    }
    pub fn busy(&self) -> bool {
        !self.samples.is_empty()
    }
    pub fn push(&mut self, frame: &[f32; 160], now: f64, independent: bool) -> Option<Utterance> {
        self.level = (frame.iter().map(|x| x * x).sum::<f32>() / 160.0).sqrt();
        if self.level >= self.threshold {
            if self.samples.is_empty() {
                self.samples.extend(self.pre_roll.drain(..));
                // Audio keeps pre-roll, but count matching uses actual speech onset.
                self.started = now;
            }
            self.hot += 1;
            self.independent_frames += usize::from(independent);
            self.quiet = 0;
        } else {
            self.quiet += 1;
        }
        if !self.samples.is_empty() || self.hot > 0 {
            self.samples.extend_from_slice(frame);
            if self.quiet >= 28 || self.samples.len() >= 40000 {
                let samples = std::mem::take(&mut self.samples);
                let valid = self.hot >= 8;
                let independent_voice = self.independent_frames >= 8;
                self.independent_frames = 0;
                self.hot = 0;
                self.quiet = 0;
                return valid.then_some(Utterance {
                    samples,
                    started: self.started,
                    independent_voice,
                });
            }
        } else {
            self.pre_roll.extend(frame);
            while self.pre_roll.len() > 3200 {
                self.pre_roll.pop_front();
            }
        }
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resampling_has_no_callback_boundary_drift() {
        for rate in [16000, 44100, 48000] {
            let mut resampler = Resampler::new(rate, 16000);
            let mut out = Vec::new();
            for _ in 0..rate {
                resampler.push(0.25, |x| out.push(x));
            }
            assert!((out.len() as i32 - 16000).abs() <= 1);
            assert!((out[out.len() - 1] - 0.25).abs() < 0.0001);
        }
    }
    #[test]
    fn silence_and_clicks_do_not_become_counts() {
        let mut vad = VoiceDetector::new(0.008);
        for n in 0..100 {
            assert!(vad.push(&[0.0; 160], n as f64 / 100.0, true).is_none());
        }
        assert!(vad.push(&[0.3; 160], 1.0, true).is_none());
        for n in 1..40 {
            assert!(
                vad.push(&[0.0; 160], 1.0 + n as f64 / 100.0, true)
                    .is_none()
            );
        }
        for n in 0..30 {
            assert!(
                vad.push(&[0.05; 160], 2.0 + n as f64 / 100.0, true)
                    .is_none()
            );
        }
        let mut utterance = None;
        for n in 0..30 {
            if let Some(value) = vad.push(&[0.0; 160], 2.3 + n as f64 / 100.0, true) {
                utterance = Some(value);
            }
        }
        let utterance = utterance.unwrap();
        assert!(utterance.started <= 2.0);
        assert!(utterance.samples.len() > 4800);
        assert!(!vad.busy());
    }
}

#[cfg(test)]
mod echo_tests {
    #[test]
    #[ignore = "requires the downloaded English speech model; never opens audio devices"]
    fn recognizes_same_number_during_simulated_speaker_playback() {
        use super::{EchoEvidence, VoiceDetector, capture_frame, echo_canceller};
        use std::sync::{Arc, atomic::AtomicBool};
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/speech-models/ggml-tiny.en-q5_1.bin");
        let recognizer = crate::speech::Recognizer::load(&path).unwrap();
        for number in 1..=10 {
            for double_talk in [false, true] {
                let mut aec = echo_canceller().unwrap();
                let mut evidence = EchoEvidence::default();
                let mut detector = VoiceDetector::new(0.008);
                let far = crate::voices::number(0, number).unwrap();
                let near = crate::voices::number(3, number).unwrap();
                let mut utterances = Vec::new();
                // Let AEC learn the room, then add an independent voice saying
                // exactly the same number alongside a 60 ms delayed speaker echo.
                for tick in 0..900 {
                    let mut render = [0.0; 160];
                    let mut capture = [0.0; 160];
                    for i in 0..160 {
                        let sample = tick * 160 + i;
                        if tick < 800 {
                            render[i] = far.get(sample % 16000).copied().unwrap_or(0.0) * 0.5;
                            capture[i] = sample
                                .checked_sub(960)
                                .and_then(|s| far.get(s % 16000))
                                .copied()
                                .unwrap_or(0.0)
                                * 0.3;
                        }
                        if double_talk {
                            capture[i] += sample
                                .checked_sub(7 * 16000 + 960)
                                .and_then(|s| near.get(s))
                                .copied()
                                .unwrap_or(0.0)
                                * 0.6;
                        }
                    }
                    evidence.render(&render);
                    let independent = evidence.independent(&capture);
                    aec.handle_render_frame(&render).unwrap();
                    if let Some(cleaned) = capture_frame(&mut aec, &capture, independent).unwrap()
                        && let Some(utterance) =
                            detector.push(&cleaned, tick as f64 / 100.0, independent)
                        && utterance.started >= 6.8
                        && utterance.independent_voice
                    {
                        utterances.push(utterance);
                    }
                }
                if double_talk {
                    let utterance = utterances
                        .iter()
                        .find(|u| (u.started - 7.0).abs() <= 0.45)
                        .expect("The overlapping human voice should survive AEC and echo checks");
                    let result = recognizer
                        .recognize(&utterance.samples, Arc::new(AtomicBool::new(false)))
                        .unwrap();
                    assert_eq!(
                        result.number,
                        Some(number),
                        "overlapping count {number}: {result:?}, started={}, length={}",
                        utterance.started,
                        utterance.samples.len()
                    );
                } else {
                    assert!(
                        utterances.is_empty(),
                        "Speaker echo must not claim a human slice"
                    );
                }
            }
        }
    }

    #[test]
    fn reference_matching_rejects_echo_but_accepts_an_overlapping_same_number() {
        use super::EchoEvidence;
        for number in [1, 5, 10] {
            let far = crate::voices::number(0, number).unwrap();
            let near = crate::voices::number(3, number).unwrap();
            for double_talk in [false, true] {
                let mut evidence = EchoEvidence::default();
                let mut independent_frames = 0;
                for tick in 0..180 {
                    let mut render = [0.0; 160];
                    let mut capture = [0.0; 160];
                    for i in 0..160 {
                        let sample = tick * 160 + i;
                        render[i] = sample
                            .checked_sub(3200)
                            .and_then(|s| far.get(s))
                            .copied()
                            .unwrap_or(0.0)
                            * 0.5;
                        capture[i] = sample
                            .checked_sub(4160)
                            .and_then(|s| far.get(s))
                            .copied()
                            .unwrap_or(0.0)
                            * 0.3;
                        if double_talk {
                            capture[i] += sample
                                .checked_sub(4160)
                                .and_then(|s| near.get(s))
                                .copied()
                                .unwrap_or(0.0)
                                * 0.6;
                        }
                    }
                    evidence.render(&render);
                    if evidence.independent(&capture)
                        && capture.iter().map(|v| v * v).sum::<f32>() > 160.0 * 0.008_f32.powi(2)
                    {
                        independent_frames += 1;
                    }
                }
                if double_talk {
                    assert!(
                        independent_frames >= 8,
                        "{number}: overlap was lost ({independent_frames} frames)"
                    );
                } else {
                    assert_eq!(
                        independent_frames, 0,
                        "{number}: pure echo looked like another person"
                    );
                }
            }
        }
    }

    #[test]
    fn speaker_echo_is_reduced_without_muting_near_end_speech() {
        use aec3::{nodes::audio::AudioFormat, pipelines::linear};
        let format = AudioFormat::ten_ms(16000, 1);
        let mut pipeline = linear::builder(format, format)
            .initial_delay_ms(60)
            .enable_gain_controller2(false)
            .build()
            .unwrap();
        let voice = crate::voices::number(0, 7).unwrap();
        let near = crate::voices::number(1, 3).unwrap();
        let mut raw_energy = 0.0_f64;
        let mut clean_energy = 0.0_f64;
        let mut near_energy = 0.0_f64;
        for tick in 0..800 {
            let mut render = [0.0; 160];
            let mut capture = [0.0; 160];
            for index in 0..160 {
                let sample = tick * 160 + index;
                if tick < 650 {
                    render[index] = voice.get(sample % 16000).copied().unwrap_or(0.0) * 0.5;
                    if sample >= 960 {
                        capture[index] =
                            voice.get((sample - 960) % 16000).copied().unwrap_or(0.0) * 0.3;
                    }
                } else if tick >= 700 {
                    capture[index] = near.get(sample - 700 * 160).copied().unwrap_or(0.0) * 0.5;
                }
            }
            pipeline.handle_render_frame(&render).unwrap();
            let mut clean = [0.0; 160];
            pipeline
                .process_capture_frame(&capture, &mut clean)
                .unwrap();
            if (500..650).contains(&tick) {
                raw_energy += capture.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                clean_energy += clean.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
            }
            if tick >= 700 {
                near_energy += clean.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
            }
        }
        assert!(
            clean_energy < raw_energy * 0.2,
            "echo raw={raw_energy} cleaned={clean_energy}"
        );
        assert!(
            near_energy > 0.5,
            "near-end speech was muted: {near_energy}"
        );
    }
}
