//! Atomic desktop flat-scene snapshot.

use crate::{
    MAX_INPUT_RECTS, MAX_NODE_CLIP_MASKS, MAX_NODE_DAMAGE_RECTS, MAX_NODE_INPUT_RECTS,
    MAX_SCENE_NODES, Rect,
    codec::{FrameWriter, MessageKind, PayloadReader},
};

/// Flat-scene node source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum SceneNodeKind {
    /// Compositor-rendered output of the connection's immutable display list.
    DisplayList = 1,
    /// Latest adopted content of one app surface.
    ForeignSurface = 2,
}

impl SceneNodeKind {
    fn parse(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::DisplayList),
            2 => Some(Self::ForeignSurface),
            _ => None,
        }
    }
}

/// One elliptical CSS corner radius in physical pixels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CornerRadius {
    /// Horizontal radius.
    pub x: u32,
    /// Vertical radius.
    pub y: u32,
}

/// One rounded ancestor clip mask applied to a scene node.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClipMask {
    /// Padding-edge rectangle in physical screen pixels.
    pub rect: Rect,
    /// Per-corner radii ordered top-left, top-right, bottom-right, bottom-left.
    pub radii: [CornerRadius; 4],
}

impl ClipMask {
    const WIRE_SIZE: usize = 48;

    fn encode(self, writer: &mut FrameWriter<'_>) -> Option<()> {
        self.rect.encode(writer)?;
        for radius in self.radii {
            writer.u32(radius.x)?;
            writer.u32(radius.y)?;
        }
        Some(())
    }

    fn parse(reader: &mut PayloadReader<'_>) -> Option<Self> {
        let rect = Rect::parse(reader)?;
        let mut radii = [CornerRadius::default(); 4];
        for radius in &mut radii {
            radius.x = reader.u32()?;
            radius.y = reader.u32()?;
        }
        Some(Self { rect, radii })
    }
}

/// One borrowed node in back-to-front paint order.
#[derive(Clone, Copy, Debug)]
pub struct SceneNode<'a> {
    /// Source interpretation.
    pub kind: SceneNodeKind,
    /// App surface whose compositor-side temporary move transform applies, or zero.
    pub window_group: u32,
    /// Display-list target id or app surface id for a foreign surface.
    pub source_id: u32,
    /// Adopted configure serial for a foreign surface; zero for a display list.
    pub configure_serial: u64,
    /// Destination bounds in physical screen pixels.
    pub bounds: Rect,
    /// Physical screen clip.
    pub clip: Rect,
    /// Rounded CSS ancestor masks applied in addition to the rectangular clip.
    pub clip_masks: ClipMasks<'a>,
    /// Conservative physical opaque rectangle.
    pub opaque: Option<Rect>,
    /// Physical input rectangles; alpha never participates in hit testing.
    pub input: Rectangles<'a>,
    /// Physical pixels requiring recomposition for this node.
    pub damage: Rectangles<'a>,
}

impl<'a> SceneNode<'a> {
    fn encode(self, writer: &mut FrameWriter<'_>) -> Option<()> {
        if self.input.len() > MAX_NODE_INPUT_RECTS
            || self.damage.len() > MAX_NODE_DAMAGE_RECTS
            || self.clip_masks.len() > MAX_NODE_CLIP_MASKS
        {
            return None;
        }
        match self.kind {
            SceneNodeKind::DisplayList if self.configure_serial != 0 => return None,
            SceneNodeKind::ForeignSurface if self.configure_serial == 0 => return None,
            _ => {}
        }
        writer.u32(self.kind as u32)?;
        writer.u32(self.window_group)?;
        writer.u32(self.source_id)?;
        writer.u64(self.configure_serial)?;
        self.bounds.encode(writer)?;
        self.clip.encode(writer)?;
        writer.u32(u32::from(self.opaque.is_some()))?;
        writer.u32(u32::try_from(self.input.len()).ok()?)?;
        writer.u32(u32::try_from(self.damage.len()).ok()?)?;
        writer.u32(u32::try_from(self.clip_masks.len()).ok()?)?;
        self.opaque.unwrap_or_default().encode(writer)?;
        for mask in self.clip_masks.iter() {
            mask.encode(writer)?;
        }
        for rectangle in self.input.iter() {
            rectangle.encode(writer)?;
        }
        for rectangle in self.damage.iter() {
            rectangle.encode(writer)?;
        }
        Some(())
    }

    fn parse(reader: &mut PayloadReader<'a>) -> Option<SceneNode<'a>> {
        let kind = SceneNodeKind::parse(reader.u32()?)?;
        let window_group = reader.u32()?;
        let source_id = reader.u32()?;
        let configure_serial = reader.u64()?;
        match kind {
            SceneNodeKind::DisplayList if configure_serial != 0 => return None,
            SceneNodeKind::ForeignSurface if configure_serial == 0 => return None,
            _ => {}
        }
        let bounds = Rect::parse(reader)?;
        let clip = Rect::parse(reader)?;
        let has_opaque = reader.u32()?;
        if has_opaque > 1 {
            return None;
        }
        let input_count = reader.u32()? as usize;
        let damage_count = reader.u32()? as usize;
        let clip_mask_count = reader.u32()? as usize;
        if input_count > MAX_NODE_INPUT_RECTS
            || damage_count > MAX_NODE_DAMAGE_RECTS
            || clip_mask_count > MAX_NODE_CLIP_MASKS
        {
            return None;
        }
        let opaque_rectangle = Rect::parse(reader)?;
        let clip_mask_bytes = reader.bytes(clip_mask_count.checked_mul(ClipMask::WIRE_SIZE)?)?;
        let input_bytes = reader.bytes(input_count.checked_mul(16)?)?;
        let damage_bytes = reader.bytes(damage_count.checked_mul(16)?)?;
        Some(SceneNode {
            kind,
            window_group,
            source_id,
            configure_serial,
            bounds,
            clip,
            clip_masks: ClipMasks::from_wire(clip_mask_bytes, clip_mask_count)?,
            opaque: (has_opaque == 1).then_some(opaque_rectangle),
            input: Rectangles::from_wire(input_bytes, input_count)?,
            damage: Rectangles::from_wire(damage_bytes, damage_count)?,
        })
    }
}

/// Borrowed validated desktop scene snapshot.
#[derive(Clone, Copy, Debug)]
pub struct SceneCommit<'a> {
    /// Monotonic desktop scene revision.
    pub revision: u64,
    /// Output configuration whose exact physical viewport this scene implements.
    pub output_serial: u64,
    /// Focused app surface, or zero when desktop itself receives keyboard input.
    pub focused_surface: u32,
    node_payload: &'a [u8],
    node_count: usize,
}

impl<'a> SceneCommit<'a> {
    /// Encodes an entire scene atomically.
    ///
    /// `nodes` are ordered back to front. The method rejects structural limits before publication.
    pub fn encode<'b>(
        bytes: &'b mut [u8],
        revision: u64,
        output_serial: u64,
        focused_surface: u32,
        nodes: &[SceneNode<'_>],
    ) -> Option<&'b [u8]> {
        if nodes.len() > MAX_SCENE_NODES
            || nodes.iter().map(|node| node.input.len()).sum::<usize>() > MAX_INPUT_RECTS
        {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::SceneCommit)?;
        writer.u64(revision)?;
        writer.u64(output_serial)?;
        writer.u32(focused_surface)?;
        writer.u32(u32::try_from(nodes.len()).ok()?)?;
        for node in nodes {
            node.encode(&mut writer)?;
        }
        writer.finish()
    }

    /// Parses and fully validates the bounded node stream.
    pub fn parse(payload: &[u8]) -> Option<SceneCommit<'_>> {
        let mut reader = PayloadReader::new(payload);
        let revision = reader.u64()?;
        let output_serial = reader.u64()?;
        let focused_surface = reader.u32()?;
        let node_count = reader.u32()? as usize;
        if node_count > MAX_SCENE_NODES {
            return None;
        }
        let node_payload = reader.bytes(payload.len().checked_sub(24)?)?;
        reader.finish()?;

        // 1. Validate every variable-length node before exposing the scene.
        // 2. Count input rectangles across nodes; per-node checks alone cannot enforce the 64 KiB scene policy.
        // 3. A second zero-allocation iterator is then safe for compositor state publication.
        let mut nodes = PayloadReader::new(node_payload);
        let mut input_total = 0usize;
        for _ in 0..node_count {
            let node = SceneNode::parse(&mut nodes)?;
            input_total = input_total.checked_add(node.input.len())?;
            if input_total > MAX_INPUT_RECTS {
                return None;
            }
        }
        nodes.finish()?;
        Some(SceneCommit {
            revision,
            output_serial,
            focused_surface,
            node_payload,
            node_count,
        })
    }

    /// Iterates the fully validated scene nodes in back-to-front order.
    pub fn nodes(self) -> SceneNodes<'a> {
        SceneNodes {
            reader: PayloadReader::new(self.node_payload),
            remaining: self.node_count,
        }
    }
}

/// Exact-size iterator over a validated scene.
pub struct SceneNodes<'a> {
    reader: PayloadReader<'a>,
    remaining: usize,
}

impl<'a> Iterator for SceneNodes<'a> {
    type Item = SceneNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        SceneNode::parse(&mut self.reader)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for SceneNodes<'_> {}

/// Zero-allocation rounded-clip view used by native encoders and wire decoders.
#[derive(Clone, Copy, Debug)]
pub enum ClipMasks<'a> {
    /// Caller-owned native masks used while encoding.
    Native(&'a [ClipMask]),
    /// Strictly validated little-endian wire bytes used while decoding.
    Wire { bytes: &'a [u8], count: usize },
}

impl<'a> ClipMasks<'a> {
    /// Creates an encoding view over native clip masks.
    pub fn from_slice(masks: &'a [ClipMask]) -> Self {
        Self::Native(masks)
    }

    fn from_wire(bytes: &'a [u8], count: usize) -> Option<Self> {
        (bytes.len() == count.checked_mul(ClipMask::WIRE_SIZE)?)
            .then_some(Self::Wire { bytes, count })
    }

    /// Returns the mask count.
    pub fn len(self) -> usize {
        match self {
            Self::Native(masks) => masks.len(),
            Self::Wire { count, .. } => count,
        }
    }

    /// Returns whether this view has no masks.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Iterates decoded mask values.
    pub fn iter(self) -> ClipMaskIterator<'a> {
        match self {
            Self::Native(masks) => ClipMaskIterator {
                inner: ClipMaskIteratorInner::Native(masks.iter()),
            },
            Self::Wire { bytes, count } => ClipMaskIterator {
                inner: ClipMaskIteratorInner::Wire {
                    reader: PayloadReader::new(bytes),
                    remaining: count,
                },
            },
        }
    }
}

/// Exact-size clip-mask iterator independent of native structure layout.
pub struct ClipMaskIterator<'a> {
    inner: ClipMaskIteratorInner<'a>,
}

enum ClipMaskIteratorInner<'a> {
    Native(std::slice::Iter<'a, ClipMask>),
    Wire {
        reader: PayloadReader<'a>,
        remaining: usize,
    },
}

impl Iterator for ClipMaskIterator<'_> {
    type Item = ClipMask;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            ClipMaskIteratorInner::Native(masks) => masks.next().copied(),
            ClipMaskIteratorInner::Wire { reader, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                ClipMask::parse(reader)
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = match &self.inner {
            ClipMaskIteratorInner::Native(masks) => masks.len(),
            ClipMaskIteratorInner::Wire { remaining, .. } => *remaining,
        };
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ClipMaskIterator<'_> {}

/// Zero-allocation rectangle view used by both native encoders and wire decoders.
#[derive(Clone, Copy, Debug)]
pub enum Rectangles<'a> {
    /// Caller-owned native rectangles used while encoding.
    Native(&'a [Rect]),
    /// Strictly validated little-endian wire bytes used while decoding.
    Wire { bytes: &'a [u8], count: usize },
}

impl<'a> Rectangles<'a> {
    /// Creates an encoding view over native rectangles.
    pub fn from_slice(rectangles: &'a [Rect]) -> Self {
        Self::Native(rectangles)
    }

    fn from_wire(bytes: &'a [u8], count: usize) -> Option<Self> {
        (bytes.len() == count.checked_mul(16)?).then_some(Self::Wire { bytes, count })
    }

    /// Returns the rectangle count.
    pub fn len(self) -> usize {
        match self {
            Self::Native(rectangles) => rectangles.len(),
            Self::Wire { count, .. } => count,
        }
    }

    /// Returns whether this view has no rectangles.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Iterates decoded rectangle values.
    pub fn iter(self) -> RectangleIterator<'a> {
        match self {
            Self::Native(rectangles) => RectangleIterator {
                inner: RectangleIteratorInner::Native(rectangles.iter()),
            },
            Self::Wire { bytes, count } => RectangleIterator {
                inner: RectangleIteratorInner::Wire {
                    reader: PayloadReader::new(bytes),
                    remaining: count,
                },
            },
        }
    }
}

/// Exact-size rectangle iterator independent of native structure layout.
pub struct RectangleIterator<'a> {
    inner: RectangleIteratorInner<'a>,
}

enum RectangleIteratorInner<'a> {
    Native(std::slice::Iter<'a, Rect>),
    Wire {
        reader: PayloadReader<'a>,
        remaining: usize,
    },
}

impl Iterator for RectangleIterator<'_> {
    type Item = Rect;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            RectangleIteratorInner::Native(rectangles) => rectangles.next().copied(),
            RectangleIteratorInner::Wire { reader, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                Rect::parse(reader)
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = match &self.inner {
            RectangleIteratorInner::Native(rectangles) => rectangles.len(),
            RectangleIteratorInner::Wire { remaining, .. } => *remaining,
        };
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for RectangleIterator<'_> {}
