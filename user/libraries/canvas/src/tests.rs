extern crate std;

use std::vec;
use std::vec::Vec;

use crate::{Color, Image, Mask, Rect, Surface};

const BLACK: Color = Color::rgb(0, 0, 0);
const WHITE: Color = Color::rgb(255, 255, 255);
const RED: Color = Color::rgb(255, 0, 0);

fn canvas(width: usize, height: usize) -> Vec<u32> {
    vec![BLACK.0; width * height]
}

fn at(pixels: &[u32], width: usize, x: usize, y: usize) -> u32 {
    pixels[y * width + x]
}

#[test]
fn rejects_buffers_that_cannot_hold_the_surface() {
    let mut pixels = canvas(4, 4);
    assert!(Surface::new(&mut pixels, 4, 4, 4).is_some());
    assert!(Surface::new(&mut pixels, 4, 5, 4).is_none());
    assert!(Surface::new(&mut pixels, 5, 4, 4).is_none());
    assert!(Surface::new(&mut pixels, -1, 4, 4).is_none());
    // 最后一行不需要补齐 stride。
    let mut padded = vec![0; 3 * 5 + 4];
    assert!(Surface::new(&mut padded, 4, 4, 5).is_some());
}

#[test]
fn fill_is_clipped_to_surface_and_clip_rect() {
    let mut pixels = canvas(6, 6);
    let mut surface = Surface::new(&mut pixels, 6, 6, 6).unwrap();
    surface.set_clip(Rect::new(1, 1, 3, 3));
    surface.fill_rect(Rect::new(-5, -5, 100, 100), WHITE);
    for y in 0..6 {
        for x in 0..6 {
            let inside = (1..4).contains(&x) && (1..4).contains(&y);
            assert_eq!(at(&pixels, 6, x, y), if inside { WHITE.0 } else { BLACK.0 });
        }
    }
}

#[test]
fn set_clip_returns_previous_and_stays_inside_surface() {
    let mut pixels = canvas(4, 4);
    let mut surface = Surface::new(&mut pixels, 4, 4, 4).unwrap();
    assert_eq!(
        surface.set_clip(Rect::new(-3, -3, 99, 99)),
        Rect::new(0, 0, 4, 4)
    );
    assert_eq!(surface.clip(), Rect::new(0, 0, 4, 4));
    surface.set_clip(Rect::new(10, 10, 2, 2));
    assert!(surface.clip().is_empty());
    surface.fill_rect(Rect::new(0, 0, 4, 4), WHITE);
    assert!(pixels.iter().all(|&p| p == BLACK.0));
}

#[test]
fn bevel_gives_bottom_right_the_mitred_corners() {
    let mut pixels = canvas(5, 5);
    let mut surface = Surface::new(&mut pixels, 5, 5, 5).unwrap();
    surface.bevel(Rect::new(0, 0, 5, 5), WHITE, RED, 1);
    assert_eq!(at(&pixels, 5, 0, 0), WHITE.0);
    assert_eq!(at(&pixels, 5, 3, 0), WHITE.0);
    assert_eq!(at(&pixels, 5, 0, 3), WHITE.0);
    assert_eq!(at(&pixels, 5, 4, 0), RED.0);
    assert_eq!(at(&pixels, 5, 0, 4), RED.0);
    assert_eq!(at(&pixels, 5, 4, 4), RED.0);
    assert_eq!(at(&pixels, 5, 2, 2), BLACK.0);
}

#[test]
fn bevel_width_two_draws_one_two_pixel_thick_ring_with_whole_mitre_blocks() {
    let mut pixels = canvas(6, 6);
    let mut surface = Surface::new(&mut pixels, 6, 6, 6).unwrap();
    surface.bevel(Rect::new(0, 0, 6, 6), WHITE, RED, 2);
    assert_eq!(at(&pixels, 6, 1, 1), WHITE.0);
    assert_eq!(at(&pixels, 6, 4, 4), RED.0);
    assert_eq!(at(&pixels, 6, 2, 2), BLACK.0);
    // 右上与左下的 2 × 2 斜接方块整块归 bottom_right。
    for (x, y) in [
        (4, 0),
        (5, 0),
        (4, 1),
        (5, 1),
        (0, 4),
        (1, 4),
        (0, 5),
        (1, 5),
    ] {
        assert_eq!(at(&pixels, 6, x, y), RED.0, "({x},{y})");
    }
    // 线宽超过半个矩形时收缩到不重叠，小矩形不会越界。
    let mut tiny = canvas(3, 3);
    Surface::new(&mut tiny, 3, 3, 3)
        .unwrap()
        .bevel(Rect::new(0, 0, 3, 3), WHITE, RED, 9);
    assert_eq!(at(&tiny, 3, 0, 0), WHITE.0);
    assert_eq!(at(&tiny, 3, 2, 2), RED.0);
}

#[test]
fn gradient_hits_both_endpoints_and_is_monotonic() {
    let mut pixels = canvas(256, 1);
    let mut surface = Surface::new(&mut pixels, 256, 1, 256).unwrap();
    surface.gradient_horizontal(Rect::new(0, 0, 256, 1), BLACK, WHITE);
    assert_eq!(pixels[0], BLACK.0);
    assert_eq!(pixels[255], WHITE.0);
    let blues: Vec<u32> = pixels.iter().map(|p| p & 0xFF).collect();
    assert!(blues.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(blues[128], 128);
}

#[test]
fn gradient_descending_channel_rounds_symmetrically() {
    let mut pixels = canvas(3, 1);
    let mut surface = Surface::new(&mut pixels, 3, 1, 3).unwrap();
    surface.gradient_horizontal(Rect::new(0, 0, 3, 1), WHITE, BLACK);
    assert_eq!(pixels[0], WHITE.0);
    // 255 → 0 的中点 127.5 向上取整，与升序渐变一致。
    assert_eq!(pixels[1], Color::rgb(128, 128, 128).0);
    assert_eq!(pixels[2], BLACK.0);
}

#[test]
fn gradient_ignores_clip_when_computing_colors() {
    let mut full = canvas(9, 1);
    Surface::new(&mut full, 9, 1, 9)
        .unwrap()
        .gradient_horizontal(Rect::new(0, 0, 9, 1), BLACK, WHITE);
    let mut clipped = canvas(9, 1);
    let mut surface = Surface::new(&mut clipped, 9, 1, 9).unwrap();
    surface.set_clip(Rect::new(4, 0, 5, 1));
    surface.gradient_horizontal(Rect::new(0, 0, 9, 1), BLACK, WHITE);
    assert_eq!(&clipped[4..], &full[4..]);
    assert_eq!(&clipped[..4], &[BLACK.0; 4]);
}

#[test]
fn checker_alternates_and_scales_with_cell() {
    let mut pixels = canvas(4, 4);
    let mut surface = Surface::new(&mut pixels, 4, 4, 4).unwrap();
    surface.checker(Rect::new(0, 0, 4, 4), 1, WHITE, RED);
    assert_eq!(at(&pixels, 4, 0, 0), WHITE.0);
    assert_eq!(at(&pixels, 4, 1, 0), RED.0);
    assert_eq!(at(&pixels, 4, 0, 1), RED.0);
    assert_eq!(at(&pixels, 4, 1, 1), WHITE.0);
    Surface::new(&mut pixels, 4, 4, 4)
        .unwrap()
        .checker(Rect::new(0, 0, 4, 4), 2, WHITE, RED);
    assert_eq!(at(&pixels, 4, 1, 1), WHITE.0);
    assert_eq!(at(&pixels, 4, 2, 1), RED.0);
    assert_eq!(at(&pixels, 4, 1, 2), RED.0);
    assert_eq!(at(&pixels, 4, 2, 2), WHITE.0);
}

#[test]
fn checker_origin_follows_rect_not_surface() {
    let mut pixels = canvas(4, 1);
    let mut surface = Surface::new(&mut pixels, 4, 1, 4).unwrap();
    surface.checker(Rect::new(1, 0, 3, 1), 1, WHITE, RED);
    assert_eq!(pixels[1], WHITE.0);
    assert_eq!(pixels[2], RED.0);
}

#[test]
fn invert_twice_restores_the_original() {
    let mut pixels: Vec<u32> = (0..16).map(|i| 0xFF00_0000 | (i * 0x010203)).collect();
    let original = pixels.clone();
    let mut surface = Surface::new(&mut pixels, 4, 4, 4).unwrap();
    surface.invert(Rect::new(1, 1, 2, 2));
    assert_eq!(pixels[5], original[5] ^ 0x00FF_FFFF);
    assert_eq!(pixels[0], original[0]);
    Surface::new(&mut pixels, 4, 4, 4)
        .unwrap()
        .invert(Rect::new(1, 1, 2, 2));
    assert_eq!(pixels, original);
}

#[test]
fn dotted_frame_is_dotted_border_only_and_self_erasing() {
    let mut pixels = canvas(6, 6);
    let mut surface = Surface::new(&mut pixels, 6, 6, 6).unwrap();
    surface.invert_dotted_frame(Rect::new(0, 0, 6, 6), 1);
    assert_eq!(at(&pixels, 6, 0, 0), WHITE.0);
    assert_eq!(at(&pixels, 6, 1, 0), BLACK.0);
    assert_eq!(at(&pixels, 6, 2, 0), WHITE.0);
    assert_eq!(at(&pixels, 6, 2, 2), BLACK.0);
    Surface::new(&mut pixels, 6, 6, 6)
        .unwrap()
        .invert_dotted_frame(Rect::new(0, 0, 6, 6), 1);
    assert!(pixels.iter().all(|&p| p == BLACK.0));
}

#[test]
fn blit_composites_premultiplied_alpha() {
    let source = [0xFF11_2233, 0x0000_0000, 0x8040_0000, 0xFF00_0000];
    let image = Image {
        pixels: &source,
        width: 2,
        height: 2,
        stride: 2,
    };
    let mut pixels = vec![0xFF00_00FF; 4];
    Surface::new(&mut pixels, 2, 2, 2)
        .unwrap()
        .blit(image, 0, 0, 1);
    assert_eq!(pixels[0], 0xFF11_2233);
    assert_eq!(pixels[1], 0xFF00_00FF);
    // 半透明红(预乘 0x40)盖在不透明蓝上：红 = 0x40，蓝 = 0xFF * 127 / 255 = 0x7F。
    assert_eq!(pixels[2], 0xFF40_007F);
    assert_eq!(pixels[3], 0xFF00_0000);
}

#[test]
fn blit_scales_by_integer_nearest_and_clips() {
    let source = [1, 2, 3, 4].map(|v| 0xFF00_0000 | v);
    let image = Image {
        pixels: &source,
        width: 2,
        height: 2,
        stride: 2,
    };
    let mut pixels = canvas(5, 5);
    Surface::new(&mut pixels, 5, 5, 5)
        .unwrap()
        .blit(image, 3, 3, 2);
    let value = |x, y| at(&pixels, 5, x, y) & 0xFF;
    assert_eq!(
        (value(3, 3), value(4, 3), value(3, 4), value(4, 4)),
        (1, 1, 1, 1)
    );
    let mut shifted = canvas(5, 5);
    Surface::new(&mut shifted, 5, 5, 5)
        .unwrap()
        .blit(image, -1, -1, 2);
    // 偏移 -1：第一个源像素只剩右下 1 格，第二个源像素占 (1,0)(1,1)(0..)。
    assert_eq!(at(&shifted, 5, 0, 0) & 0xFF, 1);
    assert_eq!(at(&shifted, 5, 1, 0) & 0xFF, 2);
    assert_eq!(at(&shifted, 5, 0, 1) & 0xFF, 3);
    assert_eq!(at(&shifted, 5, 1, 1) & 0xFF, 4);
}

#[test]
fn blit_ignores_invalid_scale_and_short_images() {
    let source = [0xFFFF_FFFF; 2];
    let short = Image {
        pixels: &source,
        width: 2,
        height: 2,
        stride: 2,
    };
    let mut pixels = canvas(2, 2);
    let mut surface = Surface::new(&mut pixels, 2, 2, 2).unwrap();
    surface.blit(short, 0, 0, 1);
    surface.blit(
        Image {
            pixels: &[0xFFFF_FFFF; 4],
            width: 2,
            height: 2,
            stride: 2,
        },
        0,
        0,
        0,
    );
    assert!(pixels.iter().all(|&p| p == BLACK.0));
}

#[test]
fn mask_blends_coverage_with_color() {
    let coverage = [0, 255, 128, 51];
    let mask = Mask {
        coverage: &coverage,
        width: 2,
        height: 2,
        stride: 2,
    };
    let mut pixels = canvas(2, 2);
    Surface::new(&mut pixels, 2, 2, 2)
        .unwrap()
        .draw_mask(mask, 0, 0, WHITE);
    assert_eq!(pixels[0], BLACK.0);
    assert_eq!(pixels[1], WHITE.0);
    assert_eq!(pixels[2], Color::rgb(128, 128, 128).0);
    assert_eq!(pixels[3], Color::rgb(51, 51, 51).0);
}

#[test]
fn mask_partially_outside_only_draws_visible_part() {
    let coverage = [255; 4];
    let mask = Mask {
        coverage: &coverage,
        width: 2,
        height: 2,
        stride: 2,
    };
    let mut pixels = canvas(2, 2);
    Surface::new(&mut pixels, 2, 2, 2)
        .unwrap()
        .draw_mask(mask, 1, 1, WHITE);
    assert_eq!(pixels, [BLACK.0, BLACK.0, BLACK.0, WHITE.0]);
}
