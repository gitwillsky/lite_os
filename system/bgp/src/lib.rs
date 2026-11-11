#![no_std]
#![feature(allocator_ext)]
//! 原生 BGP 的唯一协议状态 owner；格式归 packet-wire，传输/路由副作用由调用者按顺序执行。
extern crate alloc;
mod adj_rib_in;
mod attributes;
mod export;
mod import;
mod loc_rib;
mod negotiation;
mod path;
mod received;
mod residency;
mod routes;
mod selection;
mod session;
mod source;
pub use adj_rib_in::{AdjRibIn, ReceivedRoute};
pub use attributes::{AttributeChanges, AttributePool, RouteAttributes};
pub use export::{AdjRibOut, ExportContext, ExportError};
pub use fallible_tree::OutOfMemory;
pub use import::{ImportDecision, import_route};
pub use loc_rib::{LocRib, LocRibError, LocRibUpdate, SelectedRoute};
pub use negotiation::{Config, InvalidConfig, Negotiated, Notice, OpenOffer, prefer_outgoing};
pub use path::{Path, PathPool, PathSegment, StoredPath};
pub use received::{Aggregator, Change, Handling, NextHop, ReceivedUpdate};
pub use routes::{
    ExportAddress, ExportRoute, RoutingDomain, RoutingError, RoutingProposal, RoutingUpdate,
};
pub use selection::{
    Candidate, Peer, PeerKind, Reachability, Rejection, SelectionContext, Selector,
};
pub use session::{Effect, Effects, Input, Session, State};
pub use source::{SourceError, SourcePeer};
