//! SCM graph 的锁外 storage preparation；不复制实际 topology、counter 或 membership。
use super::*;
use crate::fallible_tree::NodeSlot;
#[derive(Default)]
pub(super) struct GraphScratch {
    pub(super) scan: Vec<Arc<GraphNode>>,
    pub(super) dfs: Vec<DfsFrame>,
    pub(super) stack: Vec<Arc<GraphNode>>,
    pub(super) members: Vec<Arc<GraphNode>>,
    pub(super) components: Vec<Component>,
    pub(super) reap: Vec<Arc<UnixSocket>>,
    pub(super) probes: Vec<Arc<UnixSocket>>,
    pub(super) roots: Vec<Arc<dyn UnixPassedFile>>,
}
impl GraphScratch {
    fn allocate(count: usize) -> Result<Self, SocketError> {
        fn vector<T>(count: usize) -> Result<Vec<T>, SocketError> {
            let mut values = Vec::new();
            values
                .try_reserve_exact(count)
                .map_err(|_| SocketError::NoMemory)?;
            Ok(values)
        }
        Ok(Self {
            scan: vector(count)?,
            dfs: vector(count)?,
            stack: vector(count)?,
            members: vector(count)?,
            components: vector(count)?,
            reap: vector(count)?,
            probes: vector(count)?,
            roots: vector(count)?,
        })
    }
    pub(super) fn capacity(&self) -> usize {
        [
            self.scan.capacity(),
            self.dfs.capacity(),
            self.stack.capacity(),
            self.members.capacity(),
            self.components.capacity(),
            self.reap.capacity(),
            self.probes.capacity(),
            self.roots.capacity(),
        ]
        .into_iter()
        .min()
        .unwrap()
    }
    pub(super) fn clear(&mut self) {
        self.scan.clear();
        self.dfs.clear();
        self.stack.clear();
        self.members.clear();
        self.components.clear();
        self.reap.clear();
        self.roots.clear();
        self.probes.clear();
    }
}
/// 每条 control batch 的节点/边/UID capability 与必要 GC scratch；消耗后锁外归还 alternatives。
pub(in crate::net::unix) struct PreparedAttach {
    pub(super) target: u64,
    pub(super) nodes: Vec<VacantEntry<u64, Arc<GraphNode>>>,
    pub(super) edges: Vec<NodeSlot<EdgeKey, GraphEdge>>,
    pub(super) uid: Option<VacantEntry<u32, usize>>,
    pub(super) scratch: Option<GraphScratch>,
}
impl PreparedAttach {
    pub(in crate::net::unix) fn new(
        uid: Option<u32>,
        sources: &[UnixNode],
        target: &Arc<UnixSocket>,
    ) -> Result<Self, SocketError> {
        let mut identities = Vec::new();
        identities
            .try_reserve_exact(sources.len().saturating_add(1))
            .map_err(|_| SocketError::NoMemory)?;
        identities.extend_from_slice(sources);
        if !sources.is_empty() {
            identities.push(target.node());
        }
        identities.sort_unstable_by_key(|node| node.id);
        identities.dedup_by_key(|node| node.id);
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(identities.len())
            .map_err(|_| SocketError::NoMemory)?;
        for node in identities {
            let existing = graph().lock().nodes.get(&node.id).cloned();
            let id = node.id;
            let object = match existing {
                Some(node) => node,
                None => GraphNode::new(node)?,
            };
            nodes.push(FallibleMap::try_prepare(id, object).map_err(|_| SocketError::NoMemory)?);
        }
        let mut edges = Vec::new();
        edges
            .try_reserve_exact(sources.len())
            .map_err(|_| SocketError::NoMemory)?;
        for _ in sources {
            edges.push(FallibleMap::try_reserve_node().map_err(|_| SocketError::NoMemory)?);
        }
        let uid_node = uid
            .map(|uid| FallibleMap::try_prepare(uid, 0).map_err(|_| SocketError::NoMemory))
            .transpose()?;
        let mut prepared = Self {
            target: target.node_id(),
            nodes,
            edges,
            uid: uid_node,
            scratch: None,
        };
        prepared.refresh_scratch()?;
        Ok(prepared)
    }
    pub(in crate::net::unix) fn target(&self) -> u64 {
        self.target
    }
    pub(in crate::net::unix) fn refresh_scratch(&mut self) -> Result<(), SocketError> {
        let count = {
            let state = graph().lock();
            let missing = self
                .nodes
                .iter()
                .filter(|node| !state.nodes.contains_key(node.key()))
                .count();
            let required = state
                .nodes
                .len()
                .checked_add(missing)
                .ok_or(SocketError::NoMemory)?;
            if state.scratch_capacity() >= required {
                return Ok(());
            }
            required.checked_mul(2).ok_or(SocketError::NoMemory)?
        };
        let scratch = GraphScratch::allocate(count)?;
        self.scratch = Some(scratch);
        Ok(())
    }
}
