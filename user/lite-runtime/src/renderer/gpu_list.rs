//! Owned LiteUI paint output lowered into the display protocol on submission.

use display_proto::{
    BorderStyle, ClipMask, CornerRadius, DisplayCommand, DisplayListCommit, Glyph, Glyphs,
    GradientStop, GradientStops, ImageRepeat, ImageSampling, MAX_MESSAGE, Rect, Size,
    TextureFormat, TextureRect,
};

use super::RenderOutput;

pub(crate) struct TextureUpload {
    pub(crate) id: u32,
    pub(crate) size: Size,
    pub(crate) format: TextureFormat,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) enum GpuCommand {
    PushGroup(u32),
    PopGroup,
    PushClip(ClipMask),
    PopClip,
    PushOpacity(f32),
    PopOpacity,
    SolidRect {
        rect: Rect,
        radii: [CornerRadius; 4],
        color: u32,
    },
    LinearGradient {
        rect: Rect,
        radii: [CornerRadius; 4],
        start: [f32; 2],
        end: [f32; 2],
        stops: Vec<GradientStop>,
    },
    Border {
        rect: Rect,
        radii: [CornerRadius; 4],
        widths: [f32; 4],
        colors: [u32; 4],
        styles: [BorderStyle; 4],
    },
    BoxShadow {
        rect: Rect,
        radii: [CornerRadius; 4],
        offset: [f32; 2],
        blur: f32,
        spread: f32,
        color: u32,
        inset: bool,
    },
    Image {
        texture_id: u32,
        source: TextureRect,
        destination: Rect,
        radii: [CornerRadius; 4],
        opacity: f32,
        sampling: ImageSampling,
        repeat: ImageRepeat,
    },
    GlyphRun {
        texture_id: u32,
        color: u32,
        offset: [f32; 2],
        blur: f32,
        glyphs: Vec<Glyph>,
    },
    BackdropBlur {
        rect: Rect,
        radii: [CornerRadius; 4],
        radius: f32,
    },
}

impl GpuCommand {
    fn borrowed(&self) -> DisplayCommand<'_> {
        match self {
            Self::PushGroup(group) => DisplayCommand::PushGroup(*group),
            Self::PopGroup => DisplayCommand::PopGroup,
            Self::PushClip(mask) => DisplayCommand::PushClip(*mask),
            Self::PopClip => DisplayCommand::PopClip,
            Self::PushOpacity(opacity) => DisplayCommand::PushOpacity(*opacity),
            Self::PopOpacity => DisplayCommand::PopOpacity,
            Self::SolidRect { rect, radii, color } => DisplayCommand::SolidRect {
                rect: *rect,
                radii: *radii,
                color: *color,
            },
            Self::LinearGradient {
                rect,
                radii,
                start,
                end,
                stops,
            } => DisplayCommand::LinearGradient {
                rect: *rect,
                radii: *radii,
                start: *start,
                end: *end,
                stops: GradientStops::from_slice(stops),
            },
            Self::Border {
                rect,
                radii,
                widths,
                colors,
                styles,
            } => DisplayCommand::Border {
                rect: *rect,
                radii: *radii,
                widths: *widths,
                colors: *colors,
                styles: *styles,
            },
            Self::BoxShadow {
                rect,
                radii,
                offset,
                blur,
                spread,
                color,
                inset,
            } => DisplayCommand::BoxShadow {
                rect: *rect,
                radii: *radii,
                offset: *offset,
                blur: *blur,
                spread: *spread,
                color: *color,
                inset: *inset,
            },
            Self::Image {
                texture_id,
                source,
                destination,
                radii,
                opacity,
                sampling,
                repeat,
            } => DisplayCommand::Image {
                texture_id: *texture_id,
                source: *source,
                destination: *destination,
                radii: *radii,
                opacity: *opacity,
                sampling: *sampling,
                repeat: *repeat,
            },
            Self::GlyphRun {
                texture_id,
                color,
                offset,
                blur,
                glyphs,
            } => DisplayCommand::GlyphRun {
                texture_id: *texture_id,
                color: *color,
                offset: *offset,
                blur: *blur,
                glyphs: Glyphs::from_slice(glyphs),
            },
            Self::BackdropBlur {
                rect,
                radii,
                radius,
            } => DisplayCommand::BackdropBlur {
                rect: *rect,
                radii: *radii,
                radius: *radius,
            },
        }
    }
}

pub(crate) struct GpuFrame {
    pub(crate) commands: Vec<GpuCommand>,
    pub(crate) uploads: Vec<TextureUpload>,
    pub(crate) output: RenderOutput,
    pub(crate) retired_textures: Vec<u32>,
    pub(crate) reuses_previous: bool,
    pub(crate) paint_changed: bool,
}

impl GpuFrame {
    pub(crate) fn encode(
        &self,
        revision: u64,
        configuration_serial: u64,
        previous_paint_revision: u64,
    ) -> Option<Vec<u8>> {
        let commands = self
            .commands
            .iter()
            .map(GpuCommand::borrowed)
            .collect::<Vec<_>>();
        let length = display_proto::DisplayListCommit::encoded_len(&commands)?;
        if length > MAX_MESSAGE {
            return None;
        }
        let mut bytes = vec![0; length];
        let written = DisplayListCommit::encode(
            &mut bytes,
            revision,
            configuration_serial,
            if self.reuses_previous {
                (previous_paint_revision != 0).then_some(previous_paint_revision)?
            } else {
                0
            },
            self.output.damage.first().copied().unwrap_or_default(),
            &commands,
        )?
        .len();
        (written == length).then_some(())?;
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use display_proto::{
        DisplayListCommit, Glyph, MAX_CONTROL_MESSAGE, MessageKind, Rect, parse_frame,
    };

    use super::{GpuCommand, GpuFrame};

    #[test]
    fn dense_terminal_viewport_encodes_beyond_control_frame_quota() {
        // The reported 3008x1692 physical output exposes a 188x52 terminal
        // grid. htop is allowed to paint every cell; the old 64 KiB outer
        // frame quota rejected this valid list after the renderer built it.
        let glyph = Glyph {
            source: Rect {
                x: 0,
                y: 0,
                width: 16,
                height: 32,
            },
            destination: Rect {
                x: 0,
                y: 0,
                width: 16,
                height: 32,
            },
        };
        let glyph_count = 188 * 52;
        let commands = (0..glyph_count)
            .collect::<Vec<_>>()
            .chunks(display_proto::MAX_GLYPHS_PER_RUN)
            .map(|chunk| GpuCommand::GlyphRun {
                texture_id: 1,
                color: 0xffff_ffff,
                offset: [0.0; 2],
                blur: 0.0,
                glyphs: vec![glyph; chunk.len()],
            })
            .collect();
        let mut output = super::super::empty_output();
        output.damage.push(Rect {
            x: 0,
            y: 0,
            width: 3008,
            height: 1692,
        });
        let frame = GpuFrame {
            commands,
            uploads: Vec::new(),
            output,
            retired_textures: Vec::new(),
            reuses_previous: false,
            paint_changed: true,
        };

        let encoded = frame.encode(1, 1, 0).expect("dense terminal list");
        assert!(encoded.len() > MAX_CONTROL_MESSAGE);
        let wire = parse_frame(&encoded).expect("large display frame");
        assert_eq!(wire.kind(), MessageKind::DisplayListCommit);
        let commit = DisplayListCommit::parse(wire.payload()).expect("valid dense display list");
        assert_eq!(
            commit
                .commands()
                .map(|command| match command {
                    display_proto::DisplayCommand::GlyphRun { glyphs, .. } => glyphs.len(),
                    _ => 0,
                })
                .sum::<usize>(),
            glyph_count,
        );
    }
}
