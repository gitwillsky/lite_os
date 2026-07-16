use crate::{Style, TextRun};

/// Generation-tagged application-local node identity.
///
/// Index zero is invalid. Reusing a slot requires a different non-zero
/// generation, so delayed events cannot address a replacement node.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct NodeId(u32);

impl NodeId {
    pub const ROOT: Self = Self::new(1, 1);

    pub const fn new(index: u16, generation: u16) -> Self {
        Self((generation as u32) << 16 | index as u32)
    }

    pub const fn index(self) -> usize {
        (self.0 & 0xffff) as usize
    }

    pub const fn generation(self) -> u16 {
        (self.0 >> 16) as u16
    }
}

/// One allocation-free retained-tree mutation.
#[derive(Clone, Copy)]
pub enum Mutation {
    Create {
        id: NodeId,
        parent: NodeId,
        style: Style,
    },
    SetStyle {
        id: NodeId,
        style: Style,
    },
    SetText {
        id: NodeId,
        text: TextRun,
    },
    Remove {
        id: NodeId,
    },
}

/// An already decoded, bounded application update.
///
/// The IPC codec owns byte validation; `Scene::commit` owns semantic validation
/// and atomic publication.
pub struct Transaction<'a> {
    pub session_epoch: u64,
    pub sequence: u64,
    pub mutations: &'a [Mutation],
}
