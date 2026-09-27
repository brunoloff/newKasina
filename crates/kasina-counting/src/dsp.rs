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
#[derive(Debug)]
pub(crate) struct Utterance {
    pub samples: Vec<f32>,
    pub started: f64,
}
#[derive(Debug)]
pub(crate) struct VoiceDetector {
    threshold: f32,
    pre_roll: VecDeque<f32>,
    samples: Vec<f32>,
    hot: usize,
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
            quiet: 0,
            started: 0.0,
            level: 0.0,
        }
    }
    pub fn busy(&self) -> bool {
        !self.samples.is_empty()
    }
    pub fn push(&mut self, frame: &[f32; 160], now: f64) -> Option<Utterance> {
        self.level = (frame.iter().map(|x| x * x).sum::<f32>() / 160.0).sqrt();
        if self.level >= self.threshold {
            if self.samples.is_empty() {
                self.samples.extend(self.pre_roll.drain(..));
                self.started = now - self.samples.len() as f64 / 16000.0;
            }
            self.hot += 1;
            self.quiet = 0;
        } else {
            self.quiet += 1;
        }
        if !self.samples.is_empty() || self.hot > 0 {
            self.samples.extend_from_slice(frame);
            if self.quiet >= 28 || self.samples.len() >= 40000 {
                let samples = std::mem::take(&mut self.samples);
                let valid = self.hot >= 8;
                self.hot = 0;
                self.quiet = 0;
                return valid.then_some(Utterance {
                    samples,
                    started: self.started,
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
            assert!(vad.push(&[0.0; 160], n as f64 / 100.0).is_none());
        }
        assert!(vad.push(&[0.3; 160], 1.0).is_none());
        for n in 1..40 {
            assert!(vad.push(&[0.0; 160], 1.0 + n as f64 / 100.0).is_none());
        }
        for n in 0..30 {
            assert!(vad.push(&[0.05; 160], 2.0 + n as f64 / 100.0).is_none());
        }
        let mut utterance = None;
        for n in 0..30 {
            if let Some(value) = vad.push(&[0.0; 160], 2.3 + n as f64 / 100.0) {
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
