use super::{Cursor, CursorError, CursorSet};

/// 与 `scripts/pack_cursors.py` 相同的布局，直接构造字节。
fn encode(entries: &[(&str, u16, u16, [u16; 4], u32)]) -> Vec<u8> {
    let mut out = b"LCS1".to_vec();
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend([0, 0]);
    for (name, frames, frame_ms, hotspots, seed) in entries {
        let mut padded = name.as_bytes().to_vec();
        padded.resize(16, 0);
        out.extend(padded);
        out.extend(frames.to_le_bytes());
        out.extend(frame_ms.to_le_bytes());
        for value in hotspots {
            out.extend(value.to_le_bytes());
        }
        for edge in [32u32, 64] {
            for frame in 0..u32::from(*frames) {
                for pixel in 0..edge * edge {
                    out.extend((seed + edge * 1000 + frame * 100 + pixel % 7).to_le_bytes());
                }
            }
        }
    }
    out
}

fn sample() -> Vec<u8> {
    encode(&[
        ("arrow", 1, 0, [0, 0, 0, 0], 0),
        ("busy", 3, 100, [16, 16, 32, 32], 0x1_0000),
    ])
}

#[test]
fn parses_names_frames_hotspots_and_both_sizes() {
    let set = CursorSet::parse(&sample()).unwrap();
    let busy = set.get("busy").unwrap();
    assert_eq!(
        (busy.hotspot(1), busy.hotspot(2), busy.hotspot(3)),
        ((16, 16), (32, 32), (32, 32))
    );
    let (edge, first) = busy.frame(1, 0);
    assert_eq!((edge, first.len()), (32, 32 * 32));
    assert_eq!(first[0], 0x1_0000 + 32_000);
    let (edge, second) = busy.frame(2, 1);
    assert_eq!(
        (edge, second.len(), second[0]),
        (64, 64 * 64, 0x1_0000 + 64_000 + 100)
    );
    assert!(set.get("missing").is_none());
}

#[test]
fn frame_index_wraps_around_the_animation() {
    let set = CursorSet::parse(&sample()).unwrap();
    let busy: &Cursor = set.get("busy").unwrap();
    assert_eq!(busy.frame(1, 3).1, busy.frame(1, 0).1);
    assert_eq!(busy.frame(1, 7).1, busy.frame(1, 1).1);
}

#[test]
fn rejects_malformed_sets_without_reading_out_of_bounds() {
    let good = sample();
    assert_eq!(CursorSet::parse(b"").err(), Some(CursorError::Truncated));
    assert_eq!(
        CursorSet::parse(b"XXXX\0\0\0\0").err(),
        Some(CursorError::BadMagic)
    );
    for cut in [5, 8, 20, 30, 100, good.len() / 2, good.len() - 1] {
        assert_eq!(
            CursorSet::parse(&good[..cut]).err(),
            Some(CursorError::Truncated),
            "cut {cut}"
        );
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(
        CursorSet::parse(&trailing).err(),
        Some(CursorError::TrailingBytes)
    );
    let mut no_frames = good.clone();
    no_frames[8 + 16] = 0;
    no_frames[8 + 17] = 0;
    assert_eq!(
        CursorSet::parse(&no_frames).err(),
        Some(CursorError::NoFrames)
    );
    let mut bad_name = good;
    bad_name[8] = 0xFF;
    assert_eq!(
        CursorSet::parse(&bad_name).err(),
        Some(CursorError::BadName)
    );
}

#[test]
fn repository_cursor_set_is_valid_and_complete() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/classic/cursors.lcs");
    let set = CursorSet::parse(&std::fs::read(path).unwrap()).unwrap();
    for name in [
        "arrow",
        "help",
        "text",
        "precision",
        "unavailable",
        "resize-ns",
        "resize-ew",
        "resize-nwse",
        "resize-nesw",
        "move",
        "link",
        "busy",
        "working",
    ] {
        assert!(set.get(name).is_some(), "{name}");
    }
    let busy = set.get("busy").unwrap();
    assert_eq!(busy.frame(1, 8).1, busy.frame(1, 0).1, "8 frames wrap");
    assert_ne!(busy.frame(1, 1).1, busy.frame(1, 0).1);
    assert_eq!(set.get("link").unwrap().hotspot(1), (6, 0));
    // 箭头至少有不透明像素，且热点落在画布内。
    let arrow = set.get("arrow").unwrap();
    assert!(arrow.frame(1, 0).1.iter().any(|&pixel| pixel >> 24 == 0xFF));
    assert!(arrow.hotspot(2).0 < 64 && arrow.hotspot(2).1 < 64);
}
