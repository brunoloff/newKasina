use anyhow::Result;
use std::io::Cursor;
/// Six prerecorded synthetic voices; indices remain stable in saved settings.
pub const NAMES: [&str; 6] = [
    "Michael · male · US",
    "Heart · female · US",
    "Puck · male · US",
    "Sarah · female · US",
    "Fenrir · male · US",
    "Bella · female · US",
];
const CLIPS: [[&[u8]; 10]; 6] = [
    [
        include_bytes!("../assets/voices/0-1.wav"),
        include_bytes!("../assets/voices/0-2.wav"),
        include_bytes!("../assets/voices/0-3.wav"),
        include_bytes!("../assets/voices/0-4.wav"),
        include_bytes!("../assets/voices/0-5.wav"),
        include_bytes!("../assets/voices/0-6.wav"),
        include_bytes!("../assets/voices/0-7.wav"),
        include_bytes!("../assets/voices/0-8.wav"),
        include_bytes!("../assets/voices/0-9.wav"),
        include_bytes!("../assets/voices/0-10.wav"),
    ],
    [
        include_bytes!("../assets/voices/1-1.wav"),
        include_bytes!("../assets/voices/1-2.wav"),
        include_bytes!("../assets/voices/1-3.wav"),
        include_bytes!("../assets/voices/1-4.wav"),
        include_bytes!("../assets/voices/1-5.wav"),
        include_bytes!("../assets/voices/1-6.wav"),
        include_bytes!("../assets/voices/1-7.wav"),
        include_bytes!("../assets/voices/1-8.wav"),
        include_bytes!("../assets/voices/1-9.wav"),
        include_bytes!("../assets/voices/1-10.wav"),
    ],
    [
        include_bytes!("../assets/voices/2-1.wav"),
        include_bytes!("../assets/voices/2-2.wav"),
        include_bytes!("../assets/voices/2-3.wav"),
        include_bytes!("../assets/voices/2-4.wav"),
        include_bytes!("../assets/voices/2-5.wav"),
        include_bytes!("../assets/voices/2-6.wav"),
        include_bytes!("../assets/voices/2-7.wav"),
        include_bytes!("../assets/voices/2-8.wav"),
        include_bytes!("../assets/voices/2-9.wav"),
        include_bytes!("../assets/voices/2-10.wav"),
    ],
    [
        include_bytes!("../assets/voices/3-1.wav"),
        include_bytes!("../assets/voices/3-2.wav"),
        include_bytes!("../assets/voices/3-3.wav"),
        include_bytes!("../assets/voices/3-4.wav"),
        include_bytes!("../assets/voices/3-5.wav"),
        include_bytes!("../assets/voices/3-6.wav"),
        include_bytes!("../assets/voices/3-7.wav"),
        include_bytes!("../assets/voices/3-8.wav"),
        include_bytes!("../assets/voices/3-9.wav"),
        include_bytes!("../assets/voices/3-10.wav"),
    ],
    [
        include_bytes!("../assets/voices/4-1.wav"),
        include_bytes!("../assets/voices/4-2.wav"),
        include_bytes!("../assets/voices/4-3.wav"),
        include_bytes!("../assets/voices/4-4.wav"),
        include_bytes!("../assets/voices/4-5.wav"),
        include_bytes!("../assets/voices/4-6.wav"),
        include_bytes!("../assets/voices/4-7.wav"),
        include_bytes!("../assets/voices/4-8.wav"),
        include_bytes!("../assets/voices/4-9.wav"),
        include_bytes!("../assets/voices/4-10.wav"),
    ],
    [
        include_bytes!("../assets/voices/5-1.wav"),
        include_bytes!("../assets/voices/5-2.wav"),
        include_bytes!("../assets/voices/5-3.wav"),
        include_bytes!("../assets/voices/5-4.wav"),
        include_bytes!("../assets/voices/5-5.wav"),
        include_bytes!("../assets/voices/5-6.wav"),
        include_bytes!("../assets/voices/5-7.wav"),
        include_bytes!("../assets/voices/5-8.wav"),
        include_bytes!("../assets/voices/5-9.wav"),
        include_bytes!("../assets/voices/5-10.wav"),
    ],
];
pub fn number(voice: usize, number: u8) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(Cursor::new(
        CLIPS[voice.min(CLIPS.len() - 1)][number.clamp(1, 10) as usize - 1],
    ))?;
    Ok(reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|sample| sample as f32 / 32768.0)
        .collect())
}
/// Mix companions on the same number at equal gain, with headroom for all voices.
pub fn together(
    speakers: crate::engine::Speakers,
    count: u8,
    voice_choices: &[usize; 4],
) -> Result<Vec<f32>> {
    let clips = speakers
        .iter()
        .filter_map(|speaker| match speaker {
            crate::engine::Speaker::Companion(index) => Some(number(voice_choices[index], count)),
            crate::engine::Speaker::You => None,
        })
        .collect::<Result<Vec<_>>>()?;
    let mut mixed = vec![0.0; clips.iter().map(Vec::len).max().unwrap_or(0)];
    let gain = 1.0 / clips.len().max(1) as f32;
    for clip in clips {
        for (out, sample) in mixed.iter_mut().zip(clip) {
            *out += sample * gain;
        }
    }
    Ok(mixed)
}
pub fn bell() -> Vec<f32> {
    (0..48000)
        .map(|i| {
            let t = i as f32 / 16000.0;
            let attack = (t * 100.0).min(1.0);
            let fade = ((3.0 - t) * 5.0).min(1.0);
            [523.25_f32, 1049.0, 1582.0]
                .iter()
                .enumerate()
                .map(|(n, hz)| {
                    (t * std::f32::consts::TAU * hz).sin() * (-t * (1.4 + n as f32 * 0.8)).exp()
                        / (n + 1) as f32
                })
                .sum::<f32>()
                * 0.35
                * attack
                * fade
        })
        .collect()
}
#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires the downloaded English speech model; never opens audio devices"]
    fn prerecorded_counts_are_recognized_offline() {
        use std::sync::{Arc, atomic::AtomicBool};
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/speech-models/ggml-tiny.en-q5_1.bin");
        let recognizer = crate::speech::Recognizer::load(&path).unwrap();
        let mut failures = Vec::new();
        for voice in 0..super::NAMES.len() {
            for number in 1..=10 {
                let clip = super::number(voice, number).unwrap();
                let result = recognizer
                    .recognize(&clip, Arc::new(AtomicBool::new(false)))
                    .unwrap();
                if result.number != Some(number) {
                    failures.push(format!("{} / {number}: {result:?}", super::NAMES[voice]));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn selected_voices_are_used_in_single_and_shared_counts() {
        use crate::engine::{Speaker, Speakers};
        let choices = [5, 4, 3, 2];
        let a = super::number(5, 7).unwrap();
        let b = super::number(4, 7).unwrap();
        let mut speakers = Speakers::from(Speaker::Companion(0));
        assert_eq!(super::together(speakers, 7, &choices).unwrap(), a);
        speakers.insert(Speaker::Companion(1));
        let mixed = super::together(speakers, 7, &choices).unwrap();
        assert_eq!(mixed.len(), a.len().max(b.len()));
        for (index, value) in mixed.iter().enumerate() {
            assert_eq!(
                *value,
                0.5 * a.get(index).copied().unwrap_or(0.0)
                    + 0.5 * b.get(index).copied().unwrap_or(0.0)
            );
        }
    }

    #[test]
    fn all_voices_are_valid_and_short() {
        for voice in 0..super::NAMES.len() {
            for count in 1..=10 {
                let reader = hound::WavReader::new(std::io::Cursor::new(
                    super::CLIPS[voice][count as usize - 1],
                ))
                .unwrap();
                assert_eq!(reader.spec().sample_rate, 16000);
                assert_eq!(reader.spec().channels, 1);
                assert_eq!(reader.spec().bits_per_sample, 16);
                let clip = super::number(voice, count).unwrap();
                assert!(
                    clip.len() > 1600 && clip.len() < 32000,
                    "{voice} {count}: {}",
                    clip.len()
                );
                assert!(clip.iter().any(|sample| sample.abs() > 0.01));
            }
        }
    }
}
