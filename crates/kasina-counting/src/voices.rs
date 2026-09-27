use anyhow::Result;
use std::io::Cursor;
const CLIPS: [[&[u8]; 10]; 4] = [
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
];
pub fn number(voice: usize, number: u8) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(Cursor::new(
        CLIPS[voice.min(3)][number.clamp(1, 10) as usize - 1],
    ))?;
    Ok(reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|sample| sample as f32 / 32768.0)
        .collect())
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
    fn all_voices_are_valid_and_short() {
        for voice in 0..4 {
            for count in 1..=10 {
                let clip = super::number(voice, count).unwrap();
                assert!(
                    clip.len() > 1600 && clip.len() < 24000,
                    "{voice} {count}: {}",
                    clip.len()
                );
                assert!(clip.iter().any(|sample| sample.abs() > 0.01));
            }
        }
    }
}
