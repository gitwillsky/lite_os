extern crate std;

use std::vec::Vec;

use super::*;

/// 仓库提交的默认画面，用来保证打包器与解码器对真实数据达成一致。
const DEFAULT_SCENE: &[u8] = include_bytes!("../../assets/classic/boot-scene.lbs");

/// 按 `LBS1` 规则手工编码：整幅 `index` 色，可附加额外 RLE 对与自定义文件头字段。
fn encode(colors: u16, fill: [u16; 4], rle: &[(u8, u8)]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"LBS1");
    for field in [640u16, 480, colors, 0] {
        data.extend_from_slice(&field.to_le_bytes());
    }
    for field in fill {
        data.extend_from_slice(&field.to_le_bytes());
    }
    data.extend_from_slice(&0x0000_299Cu32.to_le_bytes());
    for index in 0..colors {
        data.extend_from_slice(&u32::from(index).to_le_bytes());
    }
    for &(run, index) in rle {
        data.extend_from_slice(&[run, index]);
    }
    data
}

/// 307200 像素全为索引 0：1204 个 255 的游程（307020）加一个 180 的尾游程。
fn solid() -> Vec<(u8, u8)> {
    let mut rle = std::vec![(255u8, 0u8); 1204];
    rle.push((180, 0));
    rle
}

fn decode_all(scene: &Scene<'_>) -> Vec<u8> {
    let mut rows = scene.rows();
    let mut row = [0u8; WIDTH];
    let mut pixels = Vec::new();
    while rows.next_row(&mut row) {
        pixels.extend_from_slice(&row);
    }
    pixels
}

#[test]
fn committed_default_scene_parses_and_decodes_to_exactly_one_frame() {
    let scene = Scene::parse(DEFAULT_SCENE).expect("committed scene must be valid");
    assert_eq!(decode_all(&scene).len(), WIDTH * HEIGHT);
    assert_eq!(scene.fill_rect(), Rect { x: 274, y: 437, width: 163, height: 8 });
    assert_eq!(scene.fill_color(), 0x0000_299C);
}

#[test]
fn committed_default_scene_has_the_sunken_progress_slot_where_the_header_says() {
    let scene = Scene::parse(DEFAULT_SCENE).unwrap();
    let pixels = decode_all(&scene);
    let at = |x: usize, y: usize| scene.color(pixels[y * WIDTH + x]);
    // 槽外框：左上 #808080、右下 #FFFFFF；内部与底栏同为 #C0C0C0，填充只会画在内部。
    assert_eq!(at(273, 436), 0x0080_8080);
    assert_eq!(at(437, 445), 0x00FF_FFFF);
    let slot = scene.fill_rect();
    for y in slot.y..slot.y + slot.height {
        for x in slot.x..slot.x + slot.width {
            assert_eq!(at(x, y), 0x00C0_C0C0, "slot interior must be empty at ({x},{y})");
        }
    }
}

#[test]
fn rows_stop_after_the_last_row() {
    let data = encode(1, [274, 437, 163, 8], &solid());
    let scene = Scene::parse(&data).unwrap();
    let mut rows = scene.rows();
    let mut row = [9u8; WIDTH];
    for _ in 0..HEIGHT {
        assert!(rows.next_row(&mut row));
        assert!(row.iter().all(|&index| index == 0));
    }
    assert!(!rows.next_row(&mut row));
}

#[test]
fn a_run_may_cross_row_boundaries() {
    // 第一游程 255 像素、第二个 255……行边界落在游程中间，解码必须续上而不是丢像素。
    let data = encode(2, [274, 437, 163, 8], &solid());
    let scene = Scene::parse(&data).unwrap();
    assert_eq!(decode_all(&scene), std::vec![0u8; WIDTH * HEIGHT]);
}

#[test]
fn malformed_inputs_are_rejected_with_the_matching_reason() {
    let good = encode(1, [274, 437, 163, 8], &solid());
    assert_eq!(Scene::parse(&good[..10]).unwrap_err(), ParseError::Truncated);
    assert_eq!(Scene::parse(&good[..24]).unwrap_err(), ParseError::Truncated); // 缺调色板
    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert_eq!(Scene::parse(&bad_magic).unwrap_err(), ParseError::BadMagic);
    let mut wrong_width = good.clone();
    wrong_width[4] = 0x20;
    assert_eq!(Scene::parse(&wrong_width).unwrap_err(), ParseError::BadHeader);
    assert_eq!(Scene::parse(&encode(0, [274, 437, 163, 8], &[])).unwrap_err(), ParseError::BadHeader);
    assert_eq!(Scene::parse(&encode(257, [274, 437, 163, 8], &solid())).unwrap_err(), ParseError::BadHeader);
    // 槽为空或越出位图都不是合法进度槽。
    assert_eq!(Scene::parse(&encode(1, [274, 437, 0, 8], &solid())).unwrap_err(), ParseError::BadHeader);
    assert_eq!(Scene::parse(&encode(1, [600, 437, 163, 8], &solid())).unwrap_err(), ParseError::BadHeader);
    assert_eq!(Scene::parse(&encode(1, [274, 475, 163, 8], &solid())).unwrap_err(), ParseError::BadHeader);
    let mut reserved = good.clone();
    reserved[10] = 1;
    assert_eq!(Scene::parse(&reserved).unwrap_err(), ParseError::BadHeader);
}

#[test]
fn malformed_pixel_streams_are_rejected() {
    let slot = [274, 437, 163, 8];
    let mut short = solid();
    short.pop();
    assert_eq!(Scene::parse(&encode(1, slot, &short)).unwrap_err(), ParseError::BadPixels);
    let mut long = solid();
    long.push((1, 0));
    assert_eq!(Scene::parse(&encode(1, slot, &long)).unwrap_err(), ParseError::BadPixels);
    let mut zero_run = solid();
    zero_run[3].0 = 0;
    assert_eq!(Scene::parse(&encode(1, slot, &zero_run)).unwrap_err(), ParseError::BadPixels);
    let mut outside_palette = solid();
    outside_palette[7].1 = 1;
    assert_eq!(Scene::parse(&encode(1, slot, &outside_palette)).unwrap_err(), ParseError::BadPixels);
    let mut odd = encode(1, slot, &solid());
    odd.push(0);
    assert_eq!(Scene::parse(&odd).unwrap_err(), ParseError::BadPixels);
}

#[test]
fn layout_uses_the_largest_integer_scale_and_centers() {
    assert_eq!(Layout::fit(640, 480), Some(Layout { scale: 1, x: 0, y: 0 }));
    assert_eq!(Layout::fit(800, 600), Some(Layout { scale: 1, x: 80, y: 60 }));
    // 3008×1692：宽度允许 4 倍，高度只允许 3 倍，取较小者；两侧各留 (3008-1920)/2 = 544 与 (1692-1440)/2 = 126。
    assert_eq!(Layout::fit(3008, 1692), Some(Layout { scale: 3, x: 544, y: 126 }));
    assert_eq!(Layout::fit(1504, 846), Some(Layout { scale: 1, x: 432, y: 183 }));
    assert_eq!(Layout::fit(639, 480), None);
    assert_eq!(Layout::fit(640, 479), None);
    let bounds = Layout::fit(3008, 1692).unwrap().bounds();
    assert_eq!((bounds.width, bounds.height), (1920, 1440));
}

#[test]
fn progress_grows_from_the_left_of_the_slot_and_is_monotonic() {
    let scene = Scene::parse(DEFAULT_SCENE).unwrap();
    let layout = Layout::fit(3008, 1692).unwrap();
    assert_eq!(layout.progress_rect(&scene, 0), None);
    let half = layout.progress_rect(&scene, 500).unwrap();
    // 槽内部 (274,437) 起、163×8，放大 3 倍后起点 (544+822, 126+1311)，半满宽 163*3/2 = 244。
    assert_eq!(half, Rect { x: 544 + 274 * 3, y: 126 + 437 * 3, width: 244, height: 24 });
    let full = layout.progress_rect(&scene, 1000).unwrap();
    assert_eq!(full.width, 163 * 3);
    assert_eq!(layout.progress_rect(&scene, 5000), Some(full)); // 超出按满处理
    let mut previous = 0;
    for permille in 0..=1000u16 {
        let width = layout.progress_rect(&scene, permille).map_or(0, |rect| rect.width);
        assert!(width >= previous, "progress width must never shrink");
        previous = width;
    }
}
