use alloc::{boxed::Box, vec::Vec};

use crate::{
    Anchors, DrawList, Mutation, NodeId, NodeRole, Primitive, PrimitiveInfo, Rect, Style, TextRun,
    Transaction,
};

#[derive(Clone, Copy)]
struct Node {
    id: NodeId,
    parent: NodeId,
    style: Style,
    text: Option<TextRun>,
}

#[derive(Clone, Copy)]
struct Slot {
    node: Option<Node>,
    generation: u16,
}

impl Slot {
    const EMPTY: Self = Self {
        node: None,
        generation: 0,
    };
}

struct State {
    slots: Box<[Slot]>,
    count: usize,
}

/// Semantic transaction failure. No variant mutates the visible state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SceneError {
    OutOfMemory,
    WrongEpoch,
    WrongSequence,
    InvalidNode,
    DuplicateNode,
    InvalidParent,
    Cycle,
    RootMutation,
    NodeBudget,
    DrawBudget,
}

/// Unique owner of one application's retained subtree.
///
/// `active` is the only visible state. `staging` has identical fixed capacity;
/// every transaction copies and validates there, then swaps the two owners.
/// Consequently commit has no rollback branch and never allocates.
pub struct Scene {
    epoch: u64,
    next_sequence: u64,
    active: State,
    staging: State,
}

impl Scene {
    /// Preallocates both transaction states and publishes an empty root.
    pub fn try_new(
        epoch: u64,
        node_capacity: usize,
        root_style: Style,
    ) -> Result<Self, SceneError> {
        if node_capacity <= NodeId::ROOT.index() || node_capacity > u16::MAX as usize {
            return Err(SceneError::NodeBudget);
        }
        let mut active = State::try_new(node_capacity)?;
        let staging = State::try_new(node_capacity)?;
        active.slots[NodeId::ROOT.index()].node = Some(Node {
            id: NodeId::ROOT,
            parent: NodeId::ROOT,
            style: root_style,
            text: None,
        });
        active.slots[NodeId::ROOT.index()].generation = NodeId::ROOT.generation();
        active.count = 1;
        Ok(Self {
            epoch,
            next_sequence: 1,
            active,
            staging,
        })
    }

    /// Copies the published tree into new fixed states and changes only root bounds.
    ///
    /// Display resize needs an off-screen candidate while the old scanout remains
    /// live. Copying the complete owner preserves application sequence and style;
    /// mutating only root bounds prevents recovery policy from replacing app chrome.
    pub fn try_clone_resized(&self, root_bounds: crate::Rect) -> Result<Self, SceneError> {
        let capacity = self.active.slots.len();
        let mut active = State::try_new(capacity)?;
        active.copy_from(&self.active);
        active.node_mut(NodeId::ROOT)?.style.bounds = root_bounds;
        let mut staging = State::try_new(capacity)?;
        staging.copy_from(&active);
        Ok(Self {
            epoch: self.epoch,
            next_sequence: self.next_sequence,
            active,
            staging,
        })
    }

    /// Reuses the two preallocated states for a fresh application generation.
    ///
    /// A disconnected host must not leave node generations or sequence state for
    /// its replacement. Resetting both owners here keeps reconnect allocation-free
    /// and makes the next accepted transaction deterministically start at one.
    pub fn reset(&mut self, epoch: u64, root_style: Style) {
        self.active.reset_root(root_style);
        self.staging.reset_root(root_style);
        self.epoch = epoch;
        self.next_sequence = 1;
    }

    /// Validates and atomically publishes a complete application update.
    pub fn commit(&mut self, transaction: Transaction<'_>) -> Result<(), SceneError> {
        if transaction.session_epoch != self.epoch {
            return Err(SceneError::WrongEpoch);
        }
        if transaction.sequence != self.next_sequence {
            return Err(SceneError::WrongSequence);
        }
        let next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(SceneError::WrongSequence)?;
        self.staging.copy_from(&self.active);
        for mutation in transaction.mutations {
            self.staging.apply(*mutation)?;
        }
        core::mem::swap(&mut self.active, &mut self.staging);
        self.next_sequence = next_sequence;
        Ok(())
    }

    /// Emits the current scene into a caller-owned fixed DrawList.
    pub fn build_draw_list(&self, output: &mut DrawList) -> Result<(), SceneError> {
        output.clear();
        for slot in self.active.slots.iter() {
            let Some(node) = slot.node else {
                continue;
            };
            if !node.style.visible {
                continue;
            }
            let bounds = self.resolved_bounds(node, 0)?;
            if !bounds.non_empty() {
                continue;
            }
            let info = PrimitiveInfo {
                node: node.id,
                window: self.window_ancestor(node, 0)?,
                role: node.style.role,
                bounds,
            };
            output
                .push(Primitive::Rectangle {
                    info,
                    fill: node.style.background,
                    border_color: node.style.border_color,
                    border_width: node.style.border_width,
                })
                .map_err(|_| SceneError::DrawBudget)?;
            if let Some(text) = node.text {
                output
                    .push(Primitive::Text { info, run: text })
                    .map_err(|_| SceneError::DrawBudget)?;
            }
        }
        Ok(())
    }

    fn window_ancestor(&self, node: Node, depth: usize) -> Result<Option<NodeId>, SceneError> {
        if node.style.role == NodeRole::Window {
            return Ok(Some(node.id));
        }
        if node.id == NodeId::ROOT {
            return Ok(None);
        }
        if depth == self.active.slots.len() {
            return Err(SceneError::Cycle);
        }
        self.window_ancestor(self.active.node(node.parent)?, depth + 1)
    }

    fn resolved_bounds(&self, node: Node, depth: usize) -> Result<Rect, SceneError> {
        if node.id == NodeId::ROOT {
            return Ok(node.style.bounds);
        }
        if depth == self.active.slots.len() {
            return Err(SceneError::Cycle);
        }
        let parent = self.active.node(node.parent)?;
        let parent_bounds = self.resolved_bounds(parent, depth + 1)?;
        Ok(resolve(parent_bounds, node.style))
    }
}

fn resolve(parent: Rect, style: Style) -> Rect {
    let anchors = style.anchors;
    let width = if anchors.contains(Anchors::STRETCH_WIDTH) {
        parent
            .width
            .saturating_sub(style.bounds.x)
            .saturating_sub(style.bounds.width)
    } else {
        style.bounds.width
    };
    let height = if anchors.contains(Anchors::STRETCH_HEIGHT) {
        parent
            .height
            .saturating_sub(style.bounds.y)
            .saturating_sub(style.bounds.height)
    } else {
        style.bounds.height
    };
    let x = if anchors.contains(Anchors::RIGHT) {
        parent
            .width
            .saturating_sub(width)
            .saturating_sub(style.bounds.x)
    } else if anchors.contains(Anchors::CENTER_X) {
        parent
            .width
            .saturating_sub(width)
            .half()
            .saturating_add(style.bounds.x)
    } else {
        style.bounds.x
    };
    let y = if anchors.contains(Anchors::BOTTOM) {
        parent
            .height
            .saturating_sub(height)
            .saturating_sub(style.bounds.y)
    } else if anchors.contains(Anchors::CENTER_Y) {
        parent
            .height
            .saturating_sub(height)
            .half()
            .saturating_add(style.bounds.y)
    } else {
        style.bounds.y
    };
    Rect {
        x: parent.x.saturating_add(x),
        y: parent.y.saturating_add(y),
        width,
        height,
    }
}

impl State {
    fn try_new(capacity: usize) -> Result<Self, SceneError> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| SceneError::OutOfMemory)?;
        slots.resize(capacity, Slot::EMPTY);
        Ok(Self {
            slots: slots.into_boxed_slice(),
            count: 0,
        })
    }

    fn copy_from(&mut self, source: &Self) {
        self.slots.copy_from_slice(&source.slots);
        self.count = source.count;
    }

    fn reset_root(&mut self, root_style: Style) {
        self.slots.fill(Slot::EMPTY);
        self.slots[NodeId::ROOT.index()] = Slot {
            node: Some(Node {
                id: NodeId::ROOT,
                parent: NodeId::ROOT,
                style: root_style,
                text: None,
            }),
            generation: NodeId::ROOT.generation(),
        };
        self.count = 1;
    }

    fn apply(&mut self, mutation: Mutation) -> Result<(), SceneError> {
        match mutation {
            Mutation::Create { id, parent, style } => self.create(id, parent, style),
            Mutation::SetStyle { id, style } => {
                self.node_mut(id)?.style = style;
                Ok(())
            }
            Mutation::SetText { id, text } => {
                self.node_mut(id)?.text = Some(text);
                Ok(())
            }
            Mutation::Remove { id } => self.remove(id),
        }
    }

    fn create(&mut self, id: NodeId, parent: NodeId, style: Style) -> Result<(), SceneError> {
        if id.index() == 0 || id.generation() == 0 {
            return Err(SceneError::InvalidNode);
        }
        let slot = self.slot(id)?;
        if slot.node.is_some() {
            return Err(SceneError::DuplicateNode);
        }
        if slot.generation.checked_add(1) != Some(id.generation()) {
            return Err(SceneError::InvalidNode);
        }
        self.node(parent).map_err(|_| SceneError::InvalidParent)?;
        if self.count == self.slots.len() {
            return Err(SceneError::NodeBudget);
        }
        self.slots[id.index()].node = Some(Node {
            id,
            parent,
            style,
            text: None,
        });
        self.slots[id.index()].generation = id.generation();
        self.count += 1;
        Ok(())
    }

    fn remove(&mut self, id: NodeId) -> Result<(), SceneError> {
        if id == NodeId::ROOT {
            return Err(SceneError::RootMutation);
        }
        self.node(id)?;
        self.slots[id.index()].node = None;
        self.count -= 1;
        // Descendants are removed only after their parent disappears. Repeating
        // the bounded scan avoids an allocation-backed work queue while keeping
        // every surviving node connected to the root.
        for _ in 0..self.slots.len() {
            let mut changed = false;
            for index in 0..self.slots.len() {
                let Some(node) = self.slots[index].node else {
                    continue;
                };
                if self.slot(node.parent)?.node.is_none() {
                    self.slots[index].node = None;
                    self.count -= 1;
                    changed = true;
                }
            }
            if !changed {
                return Ok(());
            }
        }
        Err(SceneError::Cycle)
    }

    fn slot(&self, id: NodeId) -> Result<&Slot, SceneError> {
        self.slots.get(id.index()).ok_or(SceneError::InvalidNode)
    }

    fn node(&self, id: NodeId) -> Result<Node, SceneError> {
        let node = self.slot(id)?.node.ok_or(SceneError::InvalidNode)?;
        (node.id == id)
            .then_some(node)
            .ok_or(SceneError::InvalidNode)
    }

    fn node_mut(&mut self, id: NodeId) -> Result<&mut Node, SceneError> {
        let node = self
            .slots
            .get_mut(id.index())
            .and_then(|slot| slot.node.as_mut())
            .ok_or(SceneError::InvalidNode)?;
        (node.id == id)
            .then_some(node)
            .ok_or(SceneError::InvalidNode)
    }
}
