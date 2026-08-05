use display_proto::{
    BorderStyle, ClipMask, CornerRadius, DisplayCommand, DisplayListCommit, Glyph, Glyphs,
    GradientStop, GradientStops, ImageRepeat, ImageSampling, MessageKind, Rect, Size,
    TextureCreate, TextureFormat, TexturePublish, TextureRect, TextureWrite, parse_frame,
};

const RADII: [CornerRadius; 4] = [
    CornerRadius { x: 12, y: 10 },
    CornerRadius { x: 8, y: 10 },
    CornerRadius { x: 6, y: 7 },
    CornerRadius { x: 4, y: 5 },
];

fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[test]
fn display_list_round_trips_gpu_primitives_and_balanced_stacks() {
    let stops = [
        GradientStop {
            offset: 0.0,
            color: 0xff10_2030,
        },
        GradientStop {
            offset: 1.0,
            color: 0xff40_5060,
        },
    ];
    let glyphs = [Glyph {
        source: rect(0, 0, 12, 16),
        destination: rect(20, 30, 12, 16),
    }];
    let commands = [
        DisplayCommand::PushClip(ClipMask {
            rect: rect(0, 0, 800, 600),
            radii: RADII,
        }),
        DisplayCommand::PushOpacity(0.75),
        DisplayCommand::SolidRect {
            rect: rect(10, 20, 200, 80),
            radii: RADII,
            color: 0x8040_2010,
        },
        DisplayCommand::LinearGradient {
            rect: rect(10, 100, 200, 80),
            radii: RADII,
            start: [10.0, 100.0],
            end: [210.0, 180.0],
            stops: GradientStops::from_slice(&stops),
        },
        DisplayCommand::Border {
            rect: rect(10, 20, 200, 160),
            radii: RADII,
            widths: [1.0, 2.0, 3.0, 4.0],
            colors: [0xffff_ffff; 4],
            styles: [BorderStyle::Solid; 4],
        },
        DisplayCommand::Image {
            texture_id: 7,
            source: TextureRect {
                x: -4.5,
                y: 2.25,
                width: 72.0,
                height: 48.0,
            },
            destination: rect(32, 32, 128, 128),
            radii: RADII,
            opacity: 1.0,
            sampling: ImageSampling::Linear,
            repeat: ImageRepeat::RepeatX,
        },
        DisplayCommand::GlyphRun {
            texture_id: 9,
            color: 0xffff_ffff,
            offset: [2.0, 3.0],
            blur: 4.0,
            glyphs: Glyphs::from_slice(&glyphs),
        },
        DisplayCommand::PopOpacity,
        DisplayCommand::PopClip,
    ];
    let mut bytes = [0; 4096];
    let damage = rect(0, 0, 800, 600);
    let encoded =
        DisplayListCommit::encode(&mut bytes, 11, 13, 0, damage, &commands).expect("display list");
    let frame = parse_frame(encoded).expect("frame");
    assert_eq!(frame.kind(), MessageKind::DisplayListCommit);
    let commit = DisplayListCommit::parse(frame.payload()).expect("validated list");
    assert_eq!((commit.revision, commit.configuration_serial), (11, 13));
    assert_eq!((commit.base_revision, commit.damage), (0, damage));
    assert_eq!(commit.commands().len(), commands.len());
}

#[test]
fn display_list_rejects_unbalanced_groups_and_non_finite_values() {
    let mut bytes = [0; 256];
    assert!(
        DisplayListCommit::encode(
            &mut bytes,
            1,
            1,
            0,
            rect(0, 0, 8, 8),
            &[DisplayCommand::PushOpacity(0.5)]
        )
        .is_none()
    );
    assert!(
        DisplayListCommit::encode(
            &mut bytes,
            1,
            1,
            0,
            rect(0, 0, 8, 8),
            &[
                DisplayCommand::PushOpacity(f32::NAN),
                DisplayCommand::PopOpacity,
            ]
        )
        .is_none()
    );
}

#[test]
fn texture_upload_contract_is_tightly_packed_and_chunked() {
    let create = TextureCreate {
        texture_id: 4,
        size: Size {
            width: 64,
            height: 32,
        },
        format: TextureFormat::Bgra8Premultiplied,
        byte_len: 64 * 32 * 4,
    };
    let mut bytes = [0; 256];
    let frame = parse_frame(create.encode(&mut bytes).expect("texture create")).expect("frame");
    assert_eq!(TextureCreate::parse(frame.payload()), Some(create));

    let payload = [1, 2, 3, 4];
    let write = TextureWrite {
        texture_id: 4,
        offset: 128,
        bytes: &payload,
    };
    let frame = parse_frame(write.encode(&mut bytes).expect("texture write")).expect("frame");
    assert_eq!(TextureWrite::parse(frame.payload()), Some(write));

    let publish = TexturePublish { texture_id: 4 };
    let frame = parse_frame(publish.encode(&mut bytes).expect("publish")).expect("frame");
    assert_eq!(TexturePublish::parse(frame.payload()), Some(publish));

    assert!(
        TextureCreate {
            byte_len: create.byte_len - 1,
            ..create
        }
        .encode(&mut bytes)
        .is_none()
    );
}
