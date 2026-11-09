#![no_std]
#![feature(allocator_ext)]
//! 原生 BGP 的唯一协议状态 owner；格式归 packet-wire，传输/路由副作用由调用者按顺序执行。
extern crate alloc;
mod adj_rib_in;
mod attributes;
mod loc_rib;
mod negotiation;
mod path;
mod received;
mod residency;
mod selection;
mod session;
pub use adj_rib_in::{AdjRibIn, ReceivedRoute};
pub use attributes::{AttributePool, RouteAttributes};
pub use fallible_tree::OutOfMemory;
pub use loc_rib::{LocRib, LocRibError, LocRibUpdate, SelectedRoute};
pub use negotiation::{Config, InvalidConfig, Negotiated, Notice, prefer_outgoing};
pub use path::{Path, PathPool, PathSegment, StoredPath};
pub use received::{Aggregator, Change, Handling, NextHop, ReceivedUpdate};
pub use selection::{
    Candidate, Peer, PeerKind, Reachability, Rejection, SelectionContext, Selector,
};
pub use session::{Effect, Effects, Input, Session, State};
