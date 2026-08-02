//! Owned LiteUI paint output lowered into the display protocol on submission.

use display_proto::{
    BorderStyle, ClipMask, CornerRadius, DisplayCommand, DisplayListWriter, Glyph, Glyphs,
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
}

impl GpuFrame {
    pub(crate) fn encode(&self, revision: u64, configuration_serial: u64) -> Option<Vec<u8>> {
        let mut bytes = vec![0; MAX_MESSAGE];
        let mut writer = DisplayListWriter::new(
            &mut bytes,
            revision,
            configuration_serial,
            self.commands.len(),
        )?;
        for command in &self.commands {
            writer.push(command.borrowed())?;
        }
        let length = writer.finish()?.len();
        bytes.truncate(length);
        Some(bytes)
    }
}
