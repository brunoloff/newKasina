//! Offline recognition check; never opens a microphone or plays sound.
use kasina_counting::{speech::Recognizer, voices};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(std::env::args().nth(1).expect("model path"));
    let recognizer = Recognizer::load(&path)?;
    let mut correct = 0;
    let started = std::time::Instant::now();
    for voice in 0..4 {
        for number in 1..=10 {
            let result = recognizer.recognize(
                &voices::number(voice, number)?,
                Arc::new(AtomicBool::new(false)),
            )?;
            println!("voice {voice} count {number}: {result:?}");
            if result.number == Some(number) {
                correct += 1;
            }
        }
    }
    let silence = recognizer.recognize(&vec![0.0; 16000], Arc::new(AtomicBool::new(false)))?;
    anyhow::ensure!(silence.number.is_none(), "silence accepted: {silence:?}");
    let mut random = 7_u32;
    let noise: Vec<f32> = (0..16000)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            (random as f32 / u32::MAX as f32 - 0.5) * 0.05
        })
        .collect();
    let noise_result = recognizer.recognize(&noise, Arc::new(AtomicBool::new(false)))?;
    anyhow::ensure!(
        noise_result.number.is_none(),
        "noise accepted: {noise_result:?}"
    );
    println!(
        "Recognized {correct}/40 fixtures; silence and noise rejected; {:.2}s total",
        started.elapsed().as_secs_f64()
    );
    anyhow::ensure!(correct == 40, "some number fixtures were misrecognized");
    Ok(())
}
