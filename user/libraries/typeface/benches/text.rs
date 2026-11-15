//! Diagnostic timings for the same shaped measurement and painting used by the desktop.
use std::{hint::black_box, time::Instant};
use typeface::{FontCatalog, LogicalFont, RenderTarget};

fn main() {
    let fonts = FontCatalog::system().expect("system Pango and Fontconfig");
    for scale in [1, 2] {
        let face = fonts.resolve(
            &LogicalFont {
                face: "sans-serif".into(),
                points: 9,
                weight: 400,
            },
            scale,
            true,
        );
        let mut pixels = vec![0; 640 * 40];
        let text = "LiteOS 中文 e\u{301} • ⠋ العربية";
        let start = Instant::now();
        for _ in 0..1_000 {
            let layout = face.layout(black_box(text), false, false, None);
            black_box(layout.size());
            layout.draw(
                RenderTarget {
                    pixels: &mut pixels,
                    width: 640,
                    height: 40,
                    stride: 640,
                    clip: (0, 0, 640, 40),
                },
                0,
                0,
                0xcbd5e1,
            );
        }
        println!(
            "Pango layout + paint scale={scale}: {:.1} μs/line",
            start.elapsed().as_secs_f64() * 1_000.0
        );
    }
}
