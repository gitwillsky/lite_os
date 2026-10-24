//! 生产混音数学的诊断计时；host 浮点不能代表 kernel softfloat 或设备时序。
use std::{hint::black_box, time::Instant};

mod drivers {
    pub(crate) const PCM_PERIOD_FRAMES: usize = 256;
    pub(crate) const PCM_PERIOD_BYTES: usize = PCM_PERIOD_FRAMES * 8;
}

#[path = "../../../kernel/src/audio/mixer.rs"]
mod mixer;

fn main() {
    let mut period = [0; drivers::PCM_PERIOD_BYTES];
    for sample in period.as_chunks_mut::<4>().0 {
        sample.copy_from_slice(&0.04_f32.to_le_bytes());
    }
    let mut output = period;
    for streams in [1, 8] {
        let mut operation = || {
            let mut mix = mixer::Accumulator::new();
            for _ in 0..streams {
                mix.add(black_box(&period), black_box([mixer::volume_gain(50), 1.0]));
            }
            mix.finish(
                black_box([1.0, mixer::volume_gain(75)]),
                black_box(&mut output),
            );
        };
        for _ in 0..1000 {
            operation();
        }
        let mut measurements = [0.0; 7];
        for measurement in &mut measurements {
            let start = Instant::now();
            for _ in 0..10_000 {
                black_box(&mut operation)();
            }
            *measurement = start.elapsed().as_nanos() as f64 / 10_000.0;
        }
        measurements.sort_by(f64::total_cmp);
        println!(
            "audio mixer diagnostic: streams={streams} median_ns/period={:.1}",
            measurements[3]
        );
    }
}
