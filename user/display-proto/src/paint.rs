//! Immutable GPU display lists and staged read-only texture uploads.

use crate::{
    ClipMask, CornerRadius, MAX_DISPLAY_COMMANDS, MAX_DISPLAY_STACK_DEPTH, MAX_GLYPHS_PER_RUN,
    MAX_GRADIENT_STOPS, Rect, Size,
    codec::{FrameWriter, MessageKind, PayloadReader},
};

const PUSH_CLIP: u32 = 1;
const POP_CLIP: u32 = 2;
const PUSH_OPACITY: u32 = 3;
const POP_OPACITY: u32 = 4;
const SOLID_RECT: u32 = 5;
const LINEAR_GRADIENT: u32 = 6;
const BORDER: u32 = 7;
const BOX_SHADOW: u32 = 8;
const IMAGE: u32 = 9;
const GLYPH_RUN: u32 = 10;
const BACKDROP_BLUR: u32 = 11;
const PUSH_GROUP: u32 = 12;
const POP_GROUP: u32 = 13;

/// Texture storage interpretation shared by upload validation and GPU creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum TextureFormat {
    /// Premultiplied BGRA8 color texture.
    Bgra8Premultiplied = 1,
    /// One-channel linear coverage mask used by glyphs.
    R8 = 2,
}

impl TextureFormat {
    fn parse(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Bgra8Premultiplied),
            2 => Some(Self::R8),
            _ => None,
        }
    }

    /// Returns the exact packed row size for a texture width.
    pub fn row_bytes(self, width: u32) -> Option<u32> {
        width.checked_mul(match self {
            Self::Bgra8Premultiplied => 4,
            Self::R8 => 1,
        })
    }
}

/// Declares storage for one staged immutable texture upload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextureCreate {
    /// Non-zero connection-local identity.
    pub texture_id: u32,
    /// Exact pixel dimensions.
    pub size: Size,
    /// Packed texel format.
    pub format: TextureFormat,
    /// Exact byte length; must equal tightly packed rows.
    pub byte_len: u32,
}

impl TextureCreate {
    /// Encodes a texture declaration.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        if self.texture_id == 0
            || self.size.width == 0
            || self.size.height == 0
            || self
                .format
                .row_bytes(self.size.width)?
                .checked_mul(self.size.height)?
                != self.byte_len
        {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::TextureCreate)?;
        writer.u32(self.texture_id)?;
        self.size.encode(&mut writer)?;
        writer.u32(self.format as u32)?;
        writer.u32(self.byte_len)?;
        writer.finish()
    }

    /// Parses and validates one texture declaration.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            texture_id: reader.u32()?,
            size: Size::parse(&mut reader)?,
            format: TextureFormat::parse(reader.u32()?)?,
            byte_len: reader.u32()?,
        };
        reader.finish()?;
        let expected = message
            .format
            .row_bytes(message.size.width)?
            .checked_mul(message.size.height)?;
        (message.texture_id != 0
            && message.size.width != 0
            && message.size.height != 0
            && message.byte_len == expected)
            .then_some(message)
    }
}

/// One non-empty, non-overlapping byte range of a staged texture upload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextureWrite<'a> {
    /// Identity declared by [`TextureCreate`].
    pub texture_id: u32,
    /// Byte offset in tightly packed texture storage.
    pub offset: u32,
    /// Exact bytes copied into the staging resource.
    pub bytes: &'a [u8],
}

impl TextureWrite<'_> {
    /// Encodes one bounded upload range.
    pub fn encode(self, output: &mut [u8]) -> Option<&[u8]> {
        if self.texture_id == 0 || self.bytes.is_empty() {
            return None;
        }
        let mut writer = FrameWriter::new(output, MessageKind::TextureWrite)?;
        writer.u32(self.texture_id)?;
        writer.u32(self.offset)?;
        writer.u32(u32::try_from(self.bytes.len()).ok()?)?;
        writer.bytes(self.bytes)?;
        writer.finish()
    }

    /// Parses one exact upload range.
    pub fn parse(payload: &[u8]) -> Option<TextureWrite<'_>> {
        let mut reader = PayloadReader::new(payload);
        let texture_id = reader.u32()?;
        let offset = reader.u32()?;
        let length = reader.u32()? as usize;
        let bytes = reader.bytes(length)?;
        reader.finish()?;
        (texture_id != 0 && !bytes.is_empty()).then_some(TextureWrite {
            texture_id,
            offset,
            bytes,
        })
    }
}

macro_rules! texture_identity_message {
    ($name:ident, $kind:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name {
            /// Non-zero connection-local texture identity.
            pub texture_id: u32,
        }

        impl $name {
            /// Encodes this texture lifecycle operation.
            pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
                if self.texture_id == 0 {
                    return None;
                }
                let mut writer = FrameWriter::new(bytes, MessageKind::$kind)?;
                writer.u32(self.texture_id)?;
                writer.finish()
            }

            /// Parses this texture lifecycle operation.
            pub fn parse(payload: &[u8]) -> Option<Self> {
                let mut reader = PayloadReader::new(payload);
                let message = Self {
                    texture_id: reader.u32()?,
                };
                reader.finish()?;
                (message.texture_id != 0).then_some(message)
            }
        }
    };
}

texture_identity_message!(
    TexturePublish,
    TexturePublish,
    "Atomically publishes a completely written immutable texture."
);
texture_identity_message!(
    TextureDestroy,
    TextureDestroy,
    "Drops a published texture after no later display list references it."
);

/// CSS border line style interpreted by the GPU border shader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum BorderStyle {
    None = 0,
    Solid = 1,
    Dashed = 2,
    Dotted = 3,
    Double = 4,
    Groove = 5,
    Ridge = 6,
    Inset = 7,
    Outset = 8,
}

impl BorderStyle {
    fn parse(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::None,
            1 => Self::Solid,
            2 => Self::Dashed,
            3 => Self::Dotted,
            4 => Self::Double,
            5 => Self::Groove,
            6 => Self::Ridge,
            7 => Self::Inset,
            8 => Self::Outset,
            _ => return None,
        })
    }
}

/// Image interpolation selected by CSS `image-rendering`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ImageSampling {
    Linear = 1,
    Nearest = 2,
}

impl ImageSampling {
    fn parse(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Linear),
            2 => Some(Self::Nearest),
            _ => None,
        }
    }
}

/// CSS background tiling applied independently on the two texture axes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ImageRepeat {
    NoRepeat = 0,
    RepeatX = 1,
    RepeatY = 2,
    Repeat = 3,
}

impl ImageRepeat {
    fn parse(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::NoRepeat,
            1 => Self::RepeatX,
            2 => Self::RepeatY,
            3 => Self::Repeat,
            _ => return None,
        })
    }
}

/// Floating-point texel rectangle preserving CSS image positioning and scaling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextureRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// One premultiplied color stop with normalized CSS gradient offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    pub offset: f32,
    pub color: u32,
}

/// One glyph atlas sample and its physical destination quad.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Glyph {
    pub source: Rect,
    pub destination: Rect,
}

#[derive(Clone, Copy, Debug)]
pub enum GradientStops<'a> {
    Native(&'a [GradientStop]),
    Wire { bytes: &'a [u8], count: usize },
}

impl<'a> GradientStops<'a> {
    pub fn from_slice(stops: &[GradientStop]) -> GradientStops<'_> {
        GradientStops::Native(stops)
    }

    pub fn len(self) -> usize {
        match self {
            Self::Native(stops) => stops.len(),
            Self::Wire { count, .. } => count,
        }
    }

    /// Returns whether this gradient has no color stops.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn iter(self) -> GradientStopIterator<'a> {
        GradientStopIterator {
            inner: match self {
                Self::Native(stops) => GradientStopIteratorInner::Native(stops.iter()),
                Self::Wire { bytes, count } => GradientStopIteratorInner::Wire {
                    reader: PayloadReader::new(bytes),
                    remaining: count,
                },
            },
        }
    }
}

pub struct GradientStopIterator<'a> {
    inner: GradientStopIteratorInner<'a>,
}

enum GradientStopIteratorInner<'a> {
    Native(std::slice::Iter<'a, GradientStop>),
    Wire {
        reader: PayloadReader<'a>,
        remaining: usize,
    },
}

impl Iterator for GradientStopIterator<'_> {
    type Item = GradientStop;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            GradientStopIteratorInner::Native(stops) => stops.next().copied(),
            GradientStopIteratorInner::Wire { reader, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                Some(GradientStop {
                    offset: read_f32(reader)?,
                    color: reader.u32()?,
                })
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = match &self.inner {
            GradientStopIteratorInner::Native(stops) => stops.len(),
            GradientStopIteratorInner::Wire { remaining, .. } => *remaining,
        };
        (length, Some(length))
    }
}

impl ExactSizeIterator for GradientStopIterator<'_> {}

#[derive(Clone, Copy, Debug)]
pub enum Glyphs<'a> {
    Native(&'a [Glyph]),
    Wire { bytes: &'a [u8], count: usize },
}

impl<'a> Glyphs<'a> {
    pub fn from_slice(glyphs: &[Glyph]) -> Glyphs<'_> {
        Glyphs::Native(glyphs)
    }

    pub fn len(self) -> usize {
        match self {
            Self::Native(glyphs) => glyphs.len(),
            Self::Wire { count, .. } => count,
        }
    }

    /// Returns whether this run has no glyphs.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn iter(self) -> GlyphIterator<'a> {
        GlyphIterator {
            inner: match self {
                Self::Native(glyphs) => GlyphIteratorInner::Native(glyphs.iter()),
                Self::Wire { bytes, count } => GlyphIteratorInner::Wire {
                    reader: PayloadReader::new(bytes),
                    remaining: count,
                },
            },
        }
    }
}

pub struct GlyphIterator<'a> {
    inner: GlyphIteratorInner<'a>,
}

enum GlyphIteratorInner<'a> {
    Native(std::slice::Iter<'a, Glyph>),
    Wire {
        reader: PayloadReader<'a>,
        remaining: usize,
    },
}

impl Iterator for GlyphIterator<'_> {
    type Item = Glyph;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            GlyphIteratorInner::Native(glyphs) => glyphs.next().copied(),
            GlyphIteratorInner::Wire { reader, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                Some(Glyph {
                    source: Rect::parse(reader)?,
                    destination: Rect::parse(reader)?,
                })
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = match &self.inner {
            GlyphIteratorInner::Native(glyphs) => glyphs.len(),
            GlyphIteratorInner::Wire { remaining, .. } => *remaining,
        };
        (length, Some(length))
    }
}

impl ExactSizeIterator for GlyphIterator<'_> {}

/// One validated GPU paint operation in back-to-front order.
#[derive(Clone, Copy, Debug)]
pub enum DisplayCommand<'a> {
    /// Starts one compositor-movable desktop window group.
    PushGroup(u32),
    /// Ends the current movable window group.
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
        stops: GradientStops<'a>,
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
        glyphs: Glyphs<'a>,
    },
    BackdropBlur {
        rect: Rect,
        radii: [CornerRadius; 4],
        radius: f32,
    },
}

/// Borrowed, fully validated immutable display-list snapshot.
#[derive(Clone, Copy, Debug)]
pub struct DisplayListCommit<'a> {
    pub revision: u64,
    pub configuration_serial: u64,
    payload: &'a [u8],
    count: usize,
}

/// Streaming encoder for a caller-owned bounded display-list frame.
///
/// Each pushed command is copied immediately, so gradient-stop and glyph
/// slices only need to live for the duration of [`Self::push`].
pub struct DisplayListWriter<'a> {
    writer: FrameWriter<'a>,
    declared: usize,
    written: usize,
    clip_depth: usize,
    opacity_depth: usize,
    group_depth: usize,
}

impl<'a> DisplayListWriter<'a> {
    /// Starts an exact-count display list.
    pub fn new(
        output: &'a mut [u8],
        revision: u64,
        configuration_serial: u64,
        command_count: usize,
    ) -> Option<Self> {
        if revision == 0 || configuration_serial == 0 || command_count > MAX_DISPLAY_COMMANDS {
            return None;
        }
        let mut writer = FrameWriter::new(output, MessageKind::DisplayListCommit)?;
        writer.u64(revision)?;
        writer.u64(configuration_serial)?;
        writer.u32(u32::try_from(command_count).ok()?)?;
        Some(Self {
            writer,
            declared: command_count,
            written: 0,
            clip_depth: 0,
            opacity_depth: 0,
            group_depth: 0,
        })
    }

    /// Validates and appends one command without retaining any borrowed data.
    pub fn push(&mut self, command: DisplayCommand<'_>) -> Option<()> {
        if self.written == self.declared {
            return None;
        }
        match command {
            DisplayCommand::PushGroup(group) => {
                (group != 0 && self.group_depth == 0).then_some(())?;
                self.group_depth = 1;
            }
            DisplayCommand::PopGroup => self.group_depth = self.group_depth.checked_sub(1)?,
            DisplayCommand::PushClip(_) => {
                self.clip_depth = self.clip_depth.checked_add(1)?;
                (self.clip_depth <= MAX_DISPLAY_STACK_DEPTH).then_some(())?;
            }
            DisplayCommand::PopClip => self.clip_depth = self.clip_depth.checked_sub(1)?,
            DisplayCommand::PushOpacity(opacity) => {
                valid_unit(opacity)?;
                self.opacity_depth = self.opacity_depth.checked_add(1)?;
                (self.opacity_depth <= MAX_DISPLAY_STACK_DEPTH).then_some(())?;
            }
            DisplayCommand::PopOpacity => {
                self.opacity_depth = self.opacity_depth.checked_sub(1)?;
            }
            _ => {}
        }
        encode_command(command, &mut self.writer)?;
        self.written += 1;
        Some(())
    }

    /// Publishes only an exact-count, balanced command stream.
    pub fn finish(self) -> Option<&'a [u8]> {
        (self.written == self.declared && self.clip_depth == 0 && self.opacity_depth == 0)
            .then_some(())?;
        (self.group_depth == 0).then_some(())?;
        self.writer.finish()
    }
}

impl<'a> DisplayListCommit<'a> {
    /// Encodes an atomic display list. Stack groups must be exactly balanced.
    pub fn encode<'output>(
        output: &'output mut [u8],
        revision: u64,
        configuration_serial: u64,
        commands: &[DisplayCommand<'_>],
    ) -> Option<&'output [u8]> {
        let mut writer =
            DisplayListWriter::new(output, revision, configuration_serial, commands.len())?;
        for command in commands {
            writer.push(*command)?;
        }
        writer.finish()
    }

    /// Parses and fully validates every command before exposing an iterator.
    pub fn parse(payload: &'a [u8]) -> Option<DisplayListCommit<'a>> {
        let mut reader = PayloadReader::new(payload);
        let revision = reader.u64()?;
        let configuration_serial = reader.u64()?;
        let count = reader.u32()? as usize;
        if revision == 0 || configuration_serial == 0 || count > MAX_DISPLAY_COMMANDS {
            return None;
        }
        let commands = reader.bytes(payload.len().checked_sub(20)?)?;
        reader.finish()?;
        let mut parsed = DisplayCommands {
            reader: PayloadReader::new(commands),
            remaining: count,
        };
        validate_stack(&mut parsed)?;
        (parsed.remaining == 0).then_some(())?;
        parsed.reader.finish()?;
        Some(Self {
            revision,
            configuration_serial,
            payload: commands,
            count,
        })
    }

    pub fn commands(self) -> DisplayCommands<'a> {
        DisplayCommands {
            reader: PayloadReader::new(self.payload),
            remaining: self.count,
        }
    }
}

pub struct DisplayCommands<'a> {
    reader: PayloadReader<'a>,
    remaining: usize,
}

impl<'a> Iterator for DisplayCommands<'a> {
    type Item = DisplayCommand<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        parse_command(&mut self.reader)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for DisplayCommands<'_> {}

fn validate_stack<'a>(commands: impl Iterator<Item = DisplayCommand<'a>>) -> Option<()> {
    let mut clip_depth = 0usize;
    let mut opacity_depth = 0usize;
    let mut group_depth = 0usize;
    for command in commands {
        match command {
            DisplayCommand::PushGroup(group) => {
                (group != 0 && group_depth == 0).then_some(())?;
                group_depth = 1;
            }
            DisplayCommand::PopGroup => group_depth = group_depth.checked_sub(1)?,
            DisplayCommand::PushClip(_) => {
                clip_depth = clip_depth.checked_add(1)?;
                (clip_depth <= MAX_DISPLAY_STACK_DEPTH).then_some(())?;
            }
            DisplayCommand::PopClip => clip_depth = clip_depth.checked_sub(1)?,
            DisplayCommand::PushOpacity(opacity) => {
                valid_unit(opacity)?;
                opacity_depth = opacity_depth.checked_add(1)?;
                (opacity_depth <= MAX_DISPLAY_STACK_DEPTH).then_some(())?;
            }
            DisplayCommand::PopOpacity => opacity_depth = opacity_depth.checked_sub(1)?,
            _ => {}
        }
    }
    (clip_depth == 0 && opacity_depth == 0 && group_depth == 0).then_some(())
}

fn encode_command(command: DisplayCommand<'_>, writer: &mut FrameWriter<'_>) -> Option<()> {
    match command {
        DisplayCommand::PushGroup(group) => {
            nonzero(group)?;
            writer.u32(PUSH_GROUP)?;
            writer.u32(group)
        }
        DisplayCommand::PopGroup => writer.u32(POP_GROUP),
        DisplayCommand::PushClip(mask) => {
            writer.u32(PUSH_CLIP)?;
            encode_mask(mask, writer)
        }
        DisplayCommand::PopClip => writer.u32(POP_CLIP),
        DisplayCommand::PushOpacity(opacity) => {
            valid_unit(opacity)?;
            writer.u32(PUSH_OPACITY)?;
            write_f32(writer, opacity)
        }
        DisplayCommand::PopOpacity => writer.u32(POP_OPACITY),
        DisplayCommand::SolidRect { rect, radii, color } => {
            valid_rect(rect)?;
            writer.u32(SOLID_RECT)?;
            rect.encode(writer)?;
            encode_radii(radii, writer)?;
            writer.u32(color)
        }
        DisplayCommand::LinearGradient {
            rect,
            radii,
            start,
            end,
            stops,
        } => {
            valid_rect(rect)?;
            valid_point(start)?;
            valid_point(end)?;
            if !(2..=MAX_GRADIENT_STOPS).contains(&stops.len()) {
                return None;
            }
            writer.u32(LINEAR_GRADIENT)?;
            rect.encode(writer)?;
            encode_radii(radii, writer)?;
            for value in start.into_iter().chain(end) {
                write_f32(writer, value)?;
            }
            writer.u32(stops.len() as u32)?;
            let mut previous = 0.0;
            for (index, stop) in stops.iter().enumerate() {
                valid_unit(stop.offset)?;
                if index != 0 && stop.offset < previous {
                    return None;
                }
                previous = stop.offset;
                write_f32(writer, stop.offset)?;
                writer.u32(stop.color)?;
            }
            Some(())
        }
        DisplayCommand::Border {
            rect,
            radii,
            widths,
            colors,
            styles,
        } => {
            valid_rect(rect)?;
            writer.u32(BORDER)?;
            rect.encode(writer)?;
            encode_radii(radii, writer)?;
            for width in widths {
                valid_non_negative(width)?;
                write_f32(writer, width)?;
            }
            for color in colors {
                writer.u32(color)?;
            }
            for style in styles {
                writer.u32(style as u32)?;
            }
            Some(())
        }
        DisplayCommand::BoxShadow {
            rect,
            radii,
            offset,
            blur,
            spread,
            color,
            inset,
        } => {
            valid_rect(rect)?;
            valid_point(offset)?;
            valid_non_negative(blur)?;
            finite(spread)?;
            writer.u32(BOX_SHADOW)?;
            rect.encode(writer)?;
            encode_radii(radii, writer)?;
            write_f32(writer, offset[0])?;
            write_f32(writer, offset[1])?;
            write_f32(writer, blur)?;
            write_f32(writer, spread)?;
            writer.u32(color)?;
            writer.u32(u32::from(inset))
        }
        DisplayCommand::Image {
            texture_id,
            source,
            destination,
            radii,
            opacity,
            sampling,
            repeat,
        } => {
            if texture_id == 0 {
                return None;
            }
            valid_texture_rect(source)?;
            valid_rect(destination)?;
            valid_unit(opacity)?;
            writer.u32(IMAGE)?;
            writer.u32(texture_id)?;
            encode_texture_rect(source, writer)?;
            destination.encode(writer)?;
            encode_radii(radii, writer)?;
            write_f32(writer, opacity)?;
            writer.u32(sampling as u32)?;
            writer.u32(repeat as u32)
        }
        DisplayCommand::GlyphRun {
            texture_id,
            color,
            offset,
            blur,
            glyphs,
        } => {
            if texture_id == 0 || glyphs.is_empty() || glyphs.len() > MAX_GLYPHS_PER_RUN {
                return None;
            }
            valid_point(offset)?;
            valid_non_negative(blur)?;
            writer.u32(GLYPH_RUN)?;
            writer.u32(texture_id)?;
            writer.u32(color)?;
            write_f32(writer, offset[0])?;
            write_f32(writer, offset[1])?;
            write_f32(writer, blur)?;
            writer.u32(glyphs.len() as u32)?;
            for glyph in glyphs.iter() {
                valid_rect(glyph.source)?;
                valid_rect(glyph.destination)?;
                glyph.source.encode(writer)?;
                glyph.destination.encode(writer)?;
            }
            Some(())
        }
        DisplayCommand::BackdropBlur {
            rect,
            radii,
            radius,
        } => {
            valid_rect(rect)?;
            valid_non_negative(radius)?;
            writer.u32(BACKDROP_BLUR)?;
            rect.encode(writer)?;
            encode_radii(radii, writer)?;
            write_f32(writer, radius)
        }
    }
}

fn parse_command<'a>(reader: &mut PayloadReader<'a>) -> Option<DisplayCommand<'a>> {
    Some(match reader.u32()? {
        PUSH_GROUP => DisplayCommand::PushGroup(nonzero(reader.u32()?)?),
        POP_GROUP => DisplayCommand::PopGroup,
        PUSH_CLIP => DisplayCommand::PushClip(parse_mask(reader)?),
        POP_CLIP => DisplayCommand::PopClip,
        PUSH_OPACITY => DisplayCommand::PushOpacity(valid_unit(read_f32(reader)?)?),
        POP_OPACITY => DisplayCommand::PopOpacity,
        SOLID_RECT => DisplayCommand::SolidRect {
            rect: valid_rect(Rect::parse(reader)?)?,
            radii: parse_radii(reader)?,
            color: reader.u32()?,
        },
        LINEAR_GRADIENT => {
            let rect = valid_rect(Rect::parse(reader)?)?;
            let radii = parse_radii(reader)?;
            let start = [read_f32(reader)?, read_f32(reader)?];
            let end = [read_f32(reader)?, read_f32(reader)?];
            valid_point(start)?;
            valid_point(end)?;
            let count = reader.u32()? as usize;
            if !(2..=MAX_GRADIENT_STOPS).contains(&count) {
                return None;
            }
            let bytes = reader.bytes(count.checked_mul(8)?)?;
            let stops = GradientStops::Wire { bytes, count };
            let mut previous = 0.0;
            let mut validated = 0;
            for (index, stop) in stops.iter().enumerate() {
                valid_unit(stop.offset)?;
                if index != 0 && stop.offset < previous {
                    return None;
                }
                previous = stop.offset;
                validated += 1;
            }
            (validated == count).then_some(())?;
            DisplayCommand::LinearGradient {
                rect,
                radii,
                start,
                end,
                stops,
            }
        }
        BORDER => {
            let rect = valid_rect(Rect::parse(reader)?)?;
            let radii = parse_radii(reader)?;
            let mut widths = [0.0; 4];
            for width in &mut widths {
                *width = valid_non_negative(read_f32(reader)?)?;
            }
            let mut colors = [0; 4];
            for color in &mut colors {
                *color = reader.u32()?;
            }
            let mut styles = [BorderStyle::None; 4];
            for style in &mut styles {
                *style = BorderStyle::parse(reader.u32()?)?;
            }
            DisplayCommand::Border {
                rect,
                radii,
                widths,
                colors,
                styles,
            }
        }
        BOX_SHADOW => DisplayCommand::BoxShadow {
            rect: valid_rect(Rect::parse(reader)?)?,
            radii: parse_radii(reader)?,
            offset: valid_point([read_f32(reader)?, read_f32(reader)?])?,
            blur: valid_non_negative(read_f32(reader)?)?,
            spread: finite(read_f32(reader)?)?,
            color: reader.u32()?,
            inset: match reader.u32()? {
                0 => false,
                1 => true,
                _ => return None,
            },
        },
        IMAGE => DisplayCommand::Image {
            texture_id: nonzero(reader.u32()?)?,
            source: parse_texture_rect(reader)?,
            destination: valid_rect(Rect::parse(reader)?)?,
            radii: parse_radii(reader)?,
            opacity: valid_unit(read_f32(reader)?)?,
            sampling: ImageSampling::parse(reader.u32()?)?,
            repeat: ImageRepeat::parse(reader.u32()?)?,
        },
        GLYPH_RUN => {
            let texture_id = nonzero(reader.u32()?)?;
            let color = reader.u32()?;
            let offset = valid_point([read_f32(reader)?, read_f32(reader)?])?;
            let blur = valid_non_negative(read_f32(reader)?)?;
            let count = reader.u32()? as usize;
            if count == 0 || count > MAX_GLYPHS_PER_RUN {
                return None;
            }
            let bytes = reader.bytes(count.checked_mul(32)?)?;
            let glyphs = Glyphs::Wire { bytes, count };
            let mut validated = 0;
            for glyph in glyphs.iter() {
                valid_rect(glyph.source)?;
                valid_rect(glyph.destination)?;
                validated += 1;
            }
            (validated == count).then_some(())?;
            DisplayCommand::GlyphRun {
                texture_id,
                color,
                offset,
                blur,
                glyphs,
            }
        }
        BACKDROP_BLUR => DisplayCommand::BackdropBlur {
            rect: valid_rect(Rect::parse(reader)?)?,
            radii: parse_radii(reader)?,
            radius: valid_non_negative(read_f32(reader)?)?,
        },
        _ => return None,
    })
}

fn encode_mask(mask: ClipMask, writer: &mut FrameWriter<'_>) -> Option<()> {
    valid_rect(mask.rect)?;
    mask.rect.encode(writer)?;
    encode_radii(mask.radii, writer)
}

fn parse_mask(reader: &mut PayloadReader<'_>) -> Option<ClipMask> {
    Some(ClipMask {
        rect: valid_rect(Rect::parse(reader)?)?,
        radii: parse_radii(reader)?,
    })
}

fn encode_texture_rect(rect: TextureRect, writer: &mut FrameWriter<'_>) -> Option<()> {
    valid_texture_rect(rect)?;
    write_f32(writer, rect.x)?;
    write_f32(writer, rect.y)?;
    write_f32(writer, rect.width)?;
    write_f32(writer, rect.height)
}

fn parse_texture_rect(reader: &mut PayloadReader<'_>) -> Option<TextureRect> {
    valid_texture_rect(TextureRect {
        x: read_f32(reader)?,
        y: read_f32(reader)?,
        width: read_f32(reader)?,
        height: read_f32(reader)?,
    })
}

fn encode_radii(radii: [CornerRadius; 4], writer: &mut FrameWriter<'_>) -> Option<()> {
    for radius in radii {
        writer.u32(radius.x)?;
        writer.u32(radius.y)?;
    }
    Some(())
}

fn parse_radii(reader: &mut PayloadReader<'_>) -> Option<[CornerRadius; 4]> {
    let mut radii = [CornerRadius::default(); 4];
    for radius in &mut radii {
        radius.x = reader.u32()?;
        radius.y = reader.u32()?;
    }
    Some(radii)
}

fn write_f32(writer: &mut FrameWriter<'_>, value: f32) -> Option<()> {
    finite(value)?;
    writer.u32(value.to_bits())
}

fn read_f32(reader: &mut PayloadReader<'_>) -> Option<f32> {
    finite(f32::from_bits(reader.u32()?))
}

fn finite(value: f32) -> Option<f32> {
    value.is_finite().then_some(value)
}

fn valid_non_negative(value: f32) -> Option<f32> {
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn valid_unit(value: f32) -> Option<f32> {
    (value.is_finite() && (0.0..=1.0).contains(&value)).then_some(value)
}

fn valid_point(point: [f32; 2]) -> Option<[f32; 2]> {
    (point[0].is_finite() && point[1].is_finite()).then_some(point)
}

fn valid_rect(rect: Rect) -> Option<Rect> {
    (rect.width != 0 && rect.height != 0).then_some(rect)
}

fn valid_texture_rect(rect: TextureRect) -> Option<TextureRect> {
    (rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && rect.width > 0.0
        && rect.height > 0.0)
        .then_some(rect)
}

fn nonzero(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}
