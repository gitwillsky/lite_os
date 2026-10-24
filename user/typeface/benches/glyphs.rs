//! 字体加载与字形光栅的诊断计时。不是门禁，数值受主机影响。

use std::{hint::black_box, path::Path, time::Instant};

use typeface::Typeface;

fn main() {
    let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
    for scale in [1, 2] {
        let start = Instant::now();
        let face = Typeface::load(&fonts, scale).unwrap();
        println!(
            "typeface diagnostic: load scale={scale} {:.1} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
        let text: Vec<char> = "登录到 LiteOS 文件资源管理器 Hello, World! 任务管理器"
            .chars()
            .collect();
        for _ in 0..50 {
            for character in &text {
                black_box(face.glyph(*character, false));
            }
        }
        let mut samples = [0.0; 7];
        for sample in &mut samples {
            let start = Instant::now();
            for _ in 0..200 {
                for character in &text {
                    black_box(face.glyph(black_box(*character), false));
                }
            }
            *sample = start.elapsed().as_nanos() as f64 / (200 * text.len()) as f64;
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "typeface diagnostic: glyph scale={scale} median {:.2} us/glyph",
            samples[3] / 1000.0
        );
    }
}
