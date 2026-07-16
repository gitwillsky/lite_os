use alloc::{boxed::Box, vec::Vec};

use crate::{NodeId, NodeRole, Rect, TextRun};

/// Identity and semantics preserved through backend-neutral projection.
#[derive(Clone, Copy)]
pub struct PrimitiveInfo {
    pub node: NodeId,
    pub window: Option<NodeId>,
    pub role: NodeRole,
    pub bounds: Rect,
}

/// Backend-neutral draw primitive emitted by the retained scene.
#[derive(Clone, Copy)]
pub enum Primitive {
    Rectangle {
        info: PrimitiveInfo,
        fill: u32,
        border_color: u32,
        border_width: u8,
    },
    Text {
        info: PrimitiveInfo,
        run: TextRun,
    },
}

impl Default for Primitive {
    fn default() -> Self {
        Self::Rectangle {
            info: PrimitiveInfo {
                node: NodeId::ROOT,
                window: None,
                role: NodeRole::Normal,
                bounds: Rect::default(),
            },
            fill: 0,
            border_color: 0,
            border_width: 0,
        }
    }
}

impl Primitive {
    pub const fn info(self) -> PrimitiveInfo {
        match self {
            Self::Rectangle { info, .. } | Self::Text { info, .. } => info,
        }
    }
}

/// Fixed-capacity DrawList reused for every frame.
pub struct DrawList {
    entries: Box<[Primitive]>,
    count: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DrawError {
    OutOfMemory,
    Capacity,
}

impl DrawList {
    /// Allocates the complete draw budget before the render loop starts.
    pub fn try_with_capacity(capacity: usize) -> Result<Self, DrawError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| DrawError::OutOfMemory)?;
        entries.resize(capacity, Primitive::default());
        Ok(Self {
            entries: entries.into_boxed_slice(),
            count: 0,
        })
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }

    pub fn as_slice(&self) -> &[Primitive] {
        &self.entries[..self.count]
    }

    pub(crate) fn push(&mut self, primitive: Primitive) -> Result<(), DrawError> {
        let slot = self
            .entries
            .get_mut(self.count)
            .ok_or(DrawError::Capacity)?;
        *slot = primitive;
        self.count += 1;
        Ok(())
    }
}
