use super::*;
#[test]
fn v4_bitmap_is_bounded_and_premultiplies_alpha() {
    let mut bytes = vec![0; 126];
    bytes[..2].copy_from_slice(b"BM");
    for (offset, value) in [
        (10, 122u32),
        (14, 108),
        (18, 1),
        (22, (-1i32) as u32),
        (26, 0x0020_0001),
        (30, 3),
        (54, 0x00ff0000),
        (58, 0x0000ff00),
        (62, 0xff),
        (66, 0xff000000),
        (70, 0x73524742),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[122..].copy_from_slice(&[0, 0, 255, 128]);
    assert_eq!(
        &*Bitmap::decode(&bytes, Limits::UI).unwrap().pixels,
        &[0x80800000]
    );
    for length in 0..bytes.len() {
        assert!(Bitmap::decode(&bytes[..length], Limits::UI).is_err());
    }
    bytes[22..26].copy_from_slice(&i32::MIN.to_le_bytes());
    assert!(Bitmap::decode(&bytes, Limits::UI).is_err());

    // 同一像素格式的只读图集必须在构造 u32 视图前拒绝越界、未对齐和尺寸溢出。
    let mut packed = vec![0u8; 4100];
    packed[..8].copy_from_slice(b"LUIA0001");
    for (at, value) in [(8, 1u32), (12, 128), (128, 1), (132, 1)] {
        packed[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    packed[16..24].copy_from_slice(&4100u64.to_le_bytes());
    packed[32..41].copy_from_slice(b"pixel.bmp");
    packed[136..144].copy_from_slice(&4096u64.to_le_bytes());
    packed[4096..].copy_from_slice(&0x8080_0000u32.to_le_bytes());
    assert_eq!(atlas::validate(&packed).unwrap(), 1);
    for at in [8, 128, 132, 136, 144] {
        let mut broken = packed.clone();
        broken[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(atlas::validate(&broken).is_err());
    }
    packed[136..144].copy_from_slice(&4097u64.to_le_bytes());
    assert!(atlas::validate(&packed).is_err());
}

/// 写一个 40 字节 BITMAPINFOHEADER 的 BMP；`extra` 是调色板或位域，`data` 是像素数据。
fn info_bmp(
    width: i32,
    height: i32,
    bpp: u16,
    compression: u32,
    extra: &[u8],
    data: &[u8],
) -> Vec<u8> {
    let offset = 54 + extra.len() as u32;
    let mut bytes = b"BM".to_vec();
    for value in [offset + data.len() as u32, 0, offset, 40] {
        bytes.extend(value.to_le_bytes());
    }
    bytes.extend(width.to_le_bytes());
    bytes.extend(height.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(bpp.to_le_bytes());
    bytes.extend(compression.to_le_bytes());
    // biSizeImage、两项分辨率为零；调色板图像的 biClrUsed 声明实际条目数。
    bytes.extend([0; 12]);
    let used = if bpp <= 8 { extra.len() as u32 / 4 } else { 0 };
    bytes.extend(used.to_le_bytes());
    bytes.extend([0; 4]);
    bytes.extend(extra);
    bytes.extend(data);
    bytes
}

#[test]
fn rle8_skipped_pixels_stay_transparent_and_absolute_runs_are_word_aligned() {
    let palette = [0, 0, 0, 0, 0, 0, 255, 0, 0, 255, 0, 0];
    // 自底向上：底行 = 2×下标1、3 个绝对像素 [2,1,2]（补齐到偶数）；顶行跳过 2 列后 1×下标2。
    let data = [2, 1, 0, 3, 2, 1, 2, 0, 0, 0, 0, 2, 2, 0, 1, 2, 0, 1];
    let bitmap = Bitmap::decode(&info_bmp(5, 2, 8, BI_RLE8, &palette, &data), Limits::UI).unwrap();
    let (red, green) = (0xffff_0000, 0xff00_ff00);
    assert_eq!(
        &*bitmap.pixels,
        &[0, 0, green, 0, 0, red, red, green, red, green]
    );
    assert!(
        Bitmap::decode(
            &info_bmp(5, 2, 8, BI_RLE8, &palette, &data[..5]),
            Limits::UI
        )
        .is_err()
    );
    assert!(Bitmap::decode(&info_bmp(5, -2, 8, BI_RLE8, &palette, &data), Limits::UI).is_err());
}

#[test]
fn bitfields_expand_565_and_reject_non_contiguous_masks() {
    let masks: Vec<u8> = [0xf800u32, 0x07e0, 0x001f]
        .iter()
        .flat_map(|mask| mask.to_le_bytes())
        .collect();
    let data = [0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00, 0x00, 0x00];
    let bitmap = Bitmap::decode(
        &info_bmp(3, -1, 16, BI_BITFIELDS, &masks, &data),
        Limits::UI,
    )
    .unwrap();
    assert_eq!(&*bitmap.pixels, &[0xffff_0000, 0xff00_ff00, 0xff00_00ff]);
    let broken: Vec<u8> = [0xf00fu32, 0x07e0, 0x001f]
        .iter()
        .flat_map(|mask| mask.to_le_bytes())
        .collect();
    assert!(
        Bitmap::decode(
            &info_bmp(3, -1, 16, BI_BITFIELDS, &broken, &data),
            Limits::UI
        )
        .is_err()
    );
}

#[test]
fn jpeg_color_and_grayscale_become_opaque_rgb() {
    let color = Bitmap::decode(include_bytes!("../testdata/solid-16x8.jpg"), Limits::UI).unwrap();
    assert_eq!((color.width, color.height), (16, 8));
    assert_eq!(color.pixels[2 * 16 + 2], 0xffc8_1e27);
    let gray = Bitmap::decode(include_bytes!("../testdata/gray-8x8.jpg"), Limits::UI).unwrap();
    assert_eq!(gray.pixels[2 * 8 + 2], 0xff4d_4d4d);
    let truncated = include_bytes!("../testdata/solid-16x8.jpg");
    assert!(Bitmap::decode(&truncated[..truncated.len() / 2], Limits::UI).is_err());
}

#[test]
fn png_expands_palette_and_premultiplies_alpha_before_resizing() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(vec![255, 128, 0, 10, 20, 30]);
        encoder.set_trns(vec![128, 0]);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[0, 1])
            .unwrap();
    }
    let bitmap = Bitmap::decode_png(&bytes, Limits::UI).unwrap();
    assert_eq!(&*bitmap.pixels, &[0x8080_4000, 0]);
    let scaled = bitmap.resized(16);
    assert_eq!(
        (scaled.width, scaled.height, scaled.pixels.len()),
        (16, 16, 256)
    );
    assert_eq!(scaled.pixels[0], 0x8080_4000);
    assert_eq!(scaled.pixels[15], 0);
    assert!(Bitmap::decode_png(&bytes[..bytes.len() / 2], Limits::UI).is_err());
}

#[test]
fn png_rejects_oversized_dimensions_before_allocating_output() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2049, 1);
        encoder.set_color(png::ColorType::Grayscale);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[0; 2049])
            .unwrap();
    }
    assert_eq!(
        Bitmap::decode_png(&bytes, Limits::UI).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}
