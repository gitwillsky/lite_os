#![no_std]

extern crate alloc;

mod draw;
mod geometry;
mod grid;
mod scene;
mod style;
mod text;
mod transaction;

pub use draw::{DrawError, DrawList, Primitive, PrimitiveInfo};
pub use geometry::{Fixed, Rect};
pub use grid::{
    ATTR_BLINK, ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_INVERSE, ATTR_UNDERLINE, GridCell,
    GridSnapshot, GridUpdate, TextGrid, TextGridError,
};
pub use scene::{Scene, SceneError};
pub use style::{Anchors, NodeRole, Style};
pub use text::TextRun;
pub use transaction::{Mutation, NodeId, Transaction};
