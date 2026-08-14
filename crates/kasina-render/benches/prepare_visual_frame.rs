use std::hint::black_box;
use std::time::Instant;

use kasina_render::PreparedVisualFrame;

const ITERATIONS: u64 = 20_000_000;

fn main() {
    let started = Instant::now();
    let mut checksum = 0_u64;
    for index in 0..ITERATIONS {
        let prepared = black_box(PreparedVisualFrame::new(
            (index % 10_000) as f32 * 0.001,
            (index % 1_000) as f32 * 0.001,
            8_000 + (index % 92_000) as u32,
            [1_920.0, 1_080.0],
        ));
        let upload = prepared.upload_bytes();
        checksum ^= u64::from(black_box(upload[index as usize % upload.len()]));
        checksum ^= u64::from(black_box(prepared.instance_count()));
    }
    let elapsed = started.elapsed();
    let nanoseconds_per_frame = elapsed.as_secs_f64() * 1_000_000_000.0 / ITERATIONS as f64;
    println!("renderer CPU preparation benchmark");
    println!("iterations: {ITERATIONS}");
    println!("elapsed: {:.3} ms", elapsed.as_secs_f64() * 1_000.0);
    println!("per frame: {nanoseconds_per_frame:.3} ns");
    println!(
        "uniform upload: {} bytes",
        PreparedVisualFrame::new(0.0, 0.5, 1, [1.0, 1.0])
            .upload_bytes()
            .len()
    );
    println!("checksum: {}", black_box(checksum));
}
