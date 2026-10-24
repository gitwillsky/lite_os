//! canvas 热路径的诊断计时：整屏填充、渐变、棋盘、蒙版与 alpha blit。不是门禁，数值受主机影响。

use std::{hint::black_box, time::Instant};

use canvas::{Color, Image, Mask, Rect, Surface};

const WIDTH: i32 = 1504;
const HEIGHT: i32 = 846;

fn time(name: &str, iterations: u32, mut operation: impl FnMut()) {
    for _ in 0..iterations / 10 + 1 {
        operation();
    }
    let mut samples = [0.0; 7];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        *sample = start.elapsed().as_nanos() as f64 / f64::from(iterations);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "canvas diagnostic: {name:<28} median {:>10.1} us",
        samples[3] / 1000.0
    );
}

fn main() {
    let mut pixels = vec![0xFF3A_6EA5u32; (WIDTH * HEIGHT) as usize];
    let mut surface = Surface::new(&mut pixels, WIDTH, HEIGHT, WIDTH as usize).unwrap();
    let screen = Rect::new(0, 0, WIDTH, HEIGHT);
    let (dark, light) = (Color::rgb(0x0A, 0x24, 0x6A), Color::rgb(0xA6, 0xCA, 0xF0));
    time("fill 1504x846", 200, || {
        surface.fill_rect(black_box(screen), dark)
    });
    time("gradient 1504x19 (caption)", 2000, || {
        surface.gradient_horizontal(black_box(Rect::new(0, 0, WIDTH, 19)), dark, light);
    });
    time("checker 16x400 (scrollbar)", 5000, || {
        surface.checker(black_box(Rect::new(0, 0, 16, 400)), 1, dark, light);
    });
    let icon: Vec<u32> = (0..32 * 32)
        .map(|i| if i % 3 == 0 { 0 } else { 0xFF80_4020 })
        .collect();
    let image = Image {
        pixels: &icon,
        width: 32,
        height: 32,
        stride: 32,
    };
    time("blit 32x32 icon x100", 500, || {
        for column in 0..100 {
            surface.blit(black_box(image), column * 8, 100, 1);
        }
    });
    time("blit 32x32 icon x100 @2x", 500, || {
        for column in 0..100 {
            surface.blit(black_box(image), column * 8, 100, 2);
        }
    });
    let translucent: Vec<u32> = vec![0x8040_2010; 32 * 32];
    let alpha = Image {
        pixels: &translucent,
        width: 32,
        height: 32,
        stride: 32,
    };
    time("alpha blit 32x32 x100", 500, || {
        for column in 0..100 {
            surface.blit(black_box(alpha), column * 8, 200, 1);
        }
    });
    let coverage: Vec<u8> = (0..24 * 24).map(|i| (i * 7) as u8).collect();
    let mask = Mask {
        coverage: &coverage,
        width: 24,
        height: 24,
        stride: 24,
    };
    time("mask 24x24 glyph x100", 500, || {
        for column in 0..100 {
            surface.draw_mask(black_box(mask), column * 24, 300, Color::rgb(0, 0, 0));
        }
    });
    black_box(&pixels);
}
