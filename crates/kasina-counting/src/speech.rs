use anyhow::Result;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub struct Recognizer {
    context: WhisperContext,
}
impl std::fmt::Debug for Recognizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recognizer").finish_non_exhaustive()
    }
}
#[derive(Debug)]
pub struct Recognition {
    pub number: Option<u8>,
    pub text: String,
    pub confidence: f32,
}
impl Recognizer {
    pub fn load(path: &Path) -> Result<Self> {
        crate::model::verify(path)?;
        whisper_rs::install_logging_hooks();
        let mut parameters = WhisperContextParameters::default();
        parameters.use_gpu(false);
        Ok(Self {
            context: WhisperContext::new_with_params(path, parameters)?,
        })
    }
    pub fn recognize(&self, samples: &[f32], cancelled: Arc<AtomicBool>) -> Result<Recognition> {
        let rms = (samples.iter().map(|x| x * x).sum::<f32>() / samples.len().max(1) as f32).sqrt();
        if rms < 0.001 {
            return Ok(Recognition {
                number: None,
                text: String::new(),
                confidence: 0.0,
            });
        }
        let mut state = self.context.create_state()?;
        let mut parameters = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        parameters.set_n_threads(
            std::thread::available_parallelism().map_or(2, |n| n.get().min(4)) as i32,
        );
        parameters.set_language(Some("en"));
        parameters.set_audio_ctx(256);
        // Each utterance is a single count. Bound decoding to one word to avoid
        // repetition on very short commands and keep response latency low.
        parameters.set_max_tokens(1);
        parameters.set_temperature_inc(0.0);
        parameters.set_print_special(false);
        parameters.set_print_progress(false);
        parameters.set_print_realtime(false);
        parameters.set_print_timestamps(false);
        parameters.set_no_context(true);
        parameters.set_no_timestamps(true);
        parameters.set_single_segment(true);
        parameters.set_initial_prompt("One. Two. Three. Four. Five. Six. Seven. Eight. Nine. Ten.");
        let abort: Box<dyn FnMut() -> bool> = Box::new(move || cancelled.load(Ordering::Relaxed));
        parameters.set_abort_callback_safe::<_, Box<dyn FnMut() -> bool>>(Some(abort));
        let mut padded = vec![0.0; 3200];
        padded.extend_from_slice(samples);
        padded.resize(padded.len().max(24000) + 4800, 0.0);
        state.full(parameters, &padded)?;
        let mut text = String::new();
        let mut probability = 1.0_f32;
        for segment in state.as_iter() {
            text.push_str(segment.to_str()?);
            for index in 0..segment.n_tokens() {
                if let Some(token) = segment.get_token(index)
                    && token.to_str()?.chars().any(char::is_alphanumeric)
                    && token.token_id() < self.context.token_eot()
                {
                    probability = probability.min(token.token_probability());
                }
            }
        }
        let number = (probability >= 0.30).then(|| parse_number(&text)).flatten();
        Ok(Recognition {
            number,
            text,
            confidence: probability,
        })
    }
}
/// Reject sentences, multiple numbers, and Whisper's common silence hallucinations.
pub fn parse_number(text: &str) -> Option<u8> {
    let text = text
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_lowercase();
    match text.as_str() {
        "one" | "1" => Some(1),
        "two" | "to" | "too" | "2" => Some(2),
        "three" | "3" => Some(3),
        "four" | "for" | "4" => Some(4),
        "five" | "5" => Some(5),
        "six" | "6" => Some(6),
        "seven" | "7" => Some(7),
        "eight" | "ate" | "8" => Some(8),
        "nine" | "9" => Some(9),
        "ten" | "10" => Some(10),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn accepts_only_a_single_count() {
        for (text, expected) in [
            (" One. ", Some(1)),
            ("10!", Some(10)),
            ("thank you", None),
            ("one two", None),
            ("21", None),
            ("", None),
        ] {
            assert_eq!(super::parse_number(text), expected);
        }
    }
}
