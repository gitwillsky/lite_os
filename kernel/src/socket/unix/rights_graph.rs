use alloc::{sync::Arc, vec::Vec};
use spin::{Mutex, Once};

use crate::{fallible_tree::FallibleMap, socket::SocketError};

use super::{UnixNode, UnixSocket};

struct Edge {
    successor: Arc<GraphNode>,
    batch_id: u64,
}

struct NodeState {
    edges: Vec<Edge>,
    incoming: usize,
    // OWNER: graph lock 下唯一维护的 incident edge reference count，恒等于
    // `incoming + edges.len()`。归零时该 node 才能从 topology index 精确摘除；缺失它会让
    // 每次 detach 为寻找孤立节点扫描全图。
    references: usize,
    index: Option<usize>,
    lowlink: usize,
    on_stack: bool,
    component: usize,
}

struct GraphNode {
    node: UnixNode,
    // OWNER: GRAPH lock serializes topology; this inner lock permits stable Arc edges without
    // leaking mutable graph nodes. Removing it would require address-unstable map references.
    state: Mutex<NodeState>,
}

struct DfsFrame {
    node: Arc<GraphNode>,
    next_edge: usize,
}

struct Component {
    dead: bool,
    has_edge: bool,
    members: core::ops::Range<usize>,
}

struct RightsGraph {
    // OWNER: nodes is the sole stable AF_UNIX inflight topology index. A second registry would let
    // detach and GC disagree about edge lifetime and either leak OFDs or reap reachable queues.
    nodes: FallibleMap<u64, Arc<GraphNode>>,
    // OWNER: uid_inflight is the sole real-UID SCM_RIGHTS resource counter. Counting in Process
    // state would split one UID across processes and make RLIMIT_NOFILE enforcement bypassable.
    uid_inflight: FallibleMap<u32, usize>,
    // OWNER: scratch capacity grows before topology publication; Tarjan/reap never allocate under
    // memory pressure. Losing this invariant would make the OOM recovery mechanism itself fail.
    scan: Vec<Arc<GraphNode>>,
    dfs: Vec<DfsFrame>,
    stack: Vec<Arc<GraphNode>>,
    members: Vec<Arc<GraphNode>>,
    components: Vec<Component>,
    reap: Vec<Arc<UnixSocket>>,
}

// OWNER: all attach/detach/topology mutations are serialized here. Socket queues never take GRAPH
// while GRAPH calls into a socket, preventing graph↔socket lock inversion.
static GRAPH: Once<Mutex<RightsGraph>> = Once::new();
// OWNER: only one attach transaction may release GRAPH to reap proven cycles and retry its limit
// check. Detach deliberately bypasses this gate so revocation can make progress synchronously.
static COLLECTOR: Mutex<()> = Mutex::new(());

/// @description transport lock 内 fast attach 的内部结果；不直接泄漏为 socket errno。
pub(super) enum AttachError {
    /// 当前 UID quota 需要在 transport lock 外执行 cycle collection。
    NeedsCollection,
    /// graph publication 的可见 socket error。
    Socket(SocketError),
}

impl GraphNode {
    fn new(node: UnixNode) -> Result<Arc<Self>, SocketError> {
        Arc::try_new(Self {
            node,
            state: Mutex::new(NodeState {
                edges: Vec::new(),
                incoming: 0,
                references: 0,
                index: None,
                lowlink: 0,
                on_stack: false,
                component: 0,
            }),
        })
        .map_err(|_| SocketError::NoMemory)
    }
}

impl RightsGraph {
    const fn new() -> Self {
        Self {
            nodes: FallibleMap::new(),
            uid_inflight: FallibleMap::new(),
            scan: Vec::new(),
            dfs: Vec::new(),
            stack: Vec::new(),
            members: Vec::new(),
            components: Vec::new(),
            reap: Vec::new(),
        }
    }

    fn inflight(&self, uid: u32) -> usize {
        self.uid_inflight.get(&uid).copied().unwrap_or(0)
    }

    fn reserve_scratch(&mut self, count: usize) -> Result<(), SocketError> {
        fn reserve<T>(values: &mut Vec<T>, count: usize) -> Result<(), SocketError> {
            if values.capacity() < count {
                values
                    .try_reserve_exact(count - values.len())
                    .map_err(|_| SocketError::NoMemory)?;
            }
            Ok(())
        }

        reserve(&mut self.scan, count)?;
        reserve(&mut self.dfs, count)?;
        reserve(&mut self.stack, count)?;
        reserve(&mut self.members, count)?;
        reserve(&mut self.components, count)?;
        reserve(&mut self.reap, count)?;
        Ok(())
    }

    fn insert_node(&mut self, node: UnixNode) -> Result<bool, SocketError> {
        if self.nodes.contains_key(&node.id) {
            return Ok(false);
        }
        let id = node.id;
        self.nodes
            .try_insert(id, GraphNode::new(node)?)
            .map_err(|_| SocketError::NoMemory)?;
        Ok(true)
    }

    fn attach(
        &mut self,
        uid: u32,
        batch_id: u64,
        count: usize,
        sources: &[UnixNode],
        target: &Arc<UnixSocket>,
    ) -> Result<(), SocketError> {
        let mut unique = Vec::new();
        unique
            .try_reserve_exact(sources.len().saturating_add(1))
            .map_err(|_| SocketError::NoMemory)?;
        unique.extend_from_slice(sources);
        if !sources.is_empty() {
            unique.push(target.node());
        }
        unique.sort_unstable_by_key(|node| node.id);
        unique.dedup_by_key(|node| node.id);

        let mut inserted = Vec::new();
        inserted
            .try_reserve_exact(unique.len())
            .map_err(|_| SocketError::NoMemory)?;
        for node in unique {
            match self.insert_node(node.clone()) {
                Ok(true) => inserted.push(node.id),
                Ok(false) => {}
                Err(error) => {
                    for id in inserted {
                        self.nodes.remove(&id);
                    }
                    return Err(error);
                }
            }
        }
        if let Err(error) = self.reserve_scratch(self.nodes.len()) {
            for id in inserted {
                self.nodes.remove(&id);
            }
            return Err(error);
        }

        let mut cursor = 0;
        while cursor < sources.len() {
            let start = cursor;
            while cursor < sources.len() && sources[cursor].id == sources[start].id {
                cursor += 1;
            }
            let source = self.nodes.get(&sources[start].id).unwrap().clone();
            if source
                .state
                .lock()
                .edges
                .try_reserve_exact(cursor - start)
                .is_err()
            {
                for id in inserted {
                    self.nodes.remove(&id);
                }
                return Err(SocketError::NoMemory);
            }
        }

        let uid_was_absent = !self.uid_inflight.contains_key(&uid);
        if uid_was_absent
            && self
                .uid_inflight
                .try_insert(uid, 0)
                .map_err(|_| SocketError::NoMemory)
                .is_err()
        {
            for id in inserted {
                self.nodes.remove(&id);
            }
            return Err(SocketError::NoMemory);
        }
        let target_node =
            (!sources.is_empty()).then(|| self.nodes.get(&target.node_id()).unwrap().clone());
        for source in sources {
            let node = self
                .nodes
                .get(&source.id)
                .expect("reserved AF_UNIX source node disappeared");
            let mut state = node.state.lock();
            state.edges.push(Edge {
                successor: target_node.as_ref().unwrap().clone(),
                batch_id,
            });
            state.references = state
                .references
                .checked_add(1)
                .expect("AF_UNIX graph reference overflow");
        }
        if let Some(target_node) = target_node {
            let mut state = target_node.state.lock();
            state.incoming = state
                .incoming
                .checked_add(sources.len())
                .expect("AF_UNIX graph incoming overflow");
            state.references = state
                .references
                .checked_add(sources.len())
                .expect("AF_UNIX graph reference overflow");
        }
        *self.uid_inflight.get_mut(&uid).unwrap() += count;
        Ok(())
    }

    fn detach(&mut self, uid: u32, batch_id: u64, count: usize, sources: &[UnixNode], target: u64) {
        let mut removed = 0;
        let mut previous = None;
        for source in sources {
            if previous == Some(source.id) {
                continue;
            }
            previous = Some(source.id);
            let Some(node) = self.nodes.get(&source.id) else {
                continue;
            };
            let mut state = node.state.lock();
            let before = state.edges.len();
            state.edges.retain(|edge| edge.batch_id != batch_id);
            let source_removed = before - state.edges.len();
            state.references = state
                .references
                .checked_sub(source_removed)
                .expect("AF_UNIX source reference underflow");
            removed += source_removed;
        }
        if removed != 0
            && let Some(target) = self.nodes.get(&target)
        {
            let mut state = target.state.lock();
            state.incoming = state
                .incoming
                .checked_sub(removed)
                .expect("AF_UNIX graph incoming underflow");
            state.references = state
                .references
                .checked_sub(removed)
                .expect("AF_UNIX target reference underflow");
        }
        let inflight = self
            .uid_inflight
            .get_mut(&uid)
            .expect("attached SCM_RIGHTS UID disappeared");
        *inflight = inflight
            .checked_sub(count)
            .expect("SCM_RIGHTS UID inflight underflow");
        if *inflight == 0 {
            self.uid_inflight.remove(&uid);
        }
        previous = None;
        for source in sources {
            if previous == Some(source.id) {
                continue;
            }
            previous = Some(source.id);
            self.remove_unreferenced(source.id);
        }
        self.remove_unreferenced(target);
    }

    fn remove_unreferenced(&mut self, id: u64) {
        let remove = self.nodes.get(&id).is_some_and(|node| {
            let state = node.state.lock();
            let incident = state
                .incoming
                .checked_add(state.edges.len())
                .expect("AF_UNIX incident reference overflow");
            assert_eq!(
                state.references, incident,
                "AF_UNIX incident reference count drifted"
            );
            state.references == 0
        });
        if remove {
            assert!(
                self.nodes.remove(&id).is_some(),
                "unreferenced AF_UNIX node disappeared"
            );
        }
    }

    fn discover(&mut self, node: Arc<GraphNode>, index: &mut usize) {
        {
            let mut state = node.state.lock();
            state.index = Some(*index);
            state.lowlink = *index;
            state.on_stack = true;
        }
        *index += 1;
        self.stack.push(node.clone());
        self.dfs.push(DfsFrame { node, next_edge: 0 });
    }

    fn collect_cycles(&mut self) {
        assert!(self.reap.is_empty(), "AF_UNIX reap queue was not drained");
        self.scan.clear();
        self.scan.extend(self.nodes.values().cloned());
        for node in &self.scan {
            let mut state = node.state.lock();
            state.index = None;
            state.on_stack = false;
        }
        self.dfs.clear();
        self.stack.clear();
        self.members.clear();
        self.components.clear();
        let mut next_index = 0;

        for root_index in 0..self.scan.len() {
            let root = self.scan[root_index].clone();
            if root.state.lock().index.is_some() {
                continue;
            }
            self.discover(root, &mut next_index);
            while !self.dfs.is_empty() {
                let next = {
                    let frame = self.dfs.last_mut().unwrap();
                    let state = frame.node.state.lock();
                    let edge = state
                        .edges
                        .get(frame.next_edge)
                        .map(|edge| edge.successor.clone());
                    if edge.is_some() {
                        frame.next_edge += 1;
                    }
                    edge
                };
                if let Some(successor) = next {
                    let successor_index = successor.state.lock().index;
                    if let Some(successor_index) = successor_index {
                        if !successor.state.lock().on_stack {
                            continue;
                        }
                        let node = self.dfs.last().unwrap().node.clone();
                        let mut state = node.state.lock();
                        state.lowlink = state.lowlink.min(successor_index);
                    } else {
                        self.discover(successor, &mut next_index);
                    }
                    continue;
                }

                let finished = self.dfs.pop().unwrap().node;
                let (index, lowlink) = {
                    let state = finished.state.lock();
                    (state.index.unwrap(), state.lowlink)
                };
                if index == lowlink {
                    let component = self.components.len();
                    let start = self.members.len();
                    self.components.push(Component {
                        dead: true,
                        has_edge: false,
                        members: start..start,
                    });
                    loop {
                        let member = self.stack.pop().unwrap();
                        let same = Arc::ptr_eq(&member, &finished);
                        let mut state = member.state.lock();
                        state.on_stack = false;
                        state.component = component;
                        drop(state);
                        self.members.push(member);
                        if same {
                            break;
                        }
                    }
                    self.components[component].members.end = self.members.len();
                }
                if let Some(parent) = self.dfs.last() {
                    let mut state = parent.node.state.lock();
                    state.lowlink = state.lowlink.min(lowlink);
                }
            }
        }

        // Tarjan 按 sink→source 发布 component。edge 若跨 component，其 successor 已完整
        // 分类；一次正序 pass 即可把 live dependency 反向传播到所有可达 source SCC。
        for component in 0..self.components.len() {
            let range = self.components[component].members.clone();
            let mut dead = true;
            let mut has_edge = false;
            for member_index in range {
                let node = &self.members[member_index];
                let outgoing = node.state.lock().edges.len();
                if node
                    .node
                    .socket
                    .upgrade()
                    .is_none_or(|socket| socket.externally_rooted(outgoing))
                {
                    dead = false;
                }
                for edge_index in 0..outgoing {
                    let successor = node.state.lock().edges[edge_index].successor.clone();
                    let dependency = successor.state.lock().component;
                    if dependency == component {
                        has_edge = true;
                    } else {
                        assert!(
                            dependency < component,
                            "Tarjan component order violated AF_UNIX dependency topology"
                        );
                        if !self.components[dependency].dead {
                            dead = false;
                        }
                    }
                }
            }
            self.components[component].dead = dead;
            self.components[component].has_edge = has_edge;
        }
        // reap 使用栈式 pop，因此逆序压入，保证 dependency sink 先于 source 被清空。
        for component in self.components.iter().rev() {
            if component.dead && component.has_edge {
                for member_index in component.members.clone() {
                    if let Some(socket) = self.members[member_index].node.socket.upgrade() {
                        self.reap.push(socket);
                    }
                }
            }
        }
    }
}

fn graph() -> &'static Mutex<RightsGraph> {
    GRAPH.call_once(|| Mutex::new(RightsGraph::new()))
}

/// @description 在 transport owner lock 内尝试原子附着一个 SCM_RIGHTS batch。
/// @param uid sender 的 real UID；所有 Process 共享同一 inflight quota。
/// @param limit sender 在 sendmsg 时捕获的 RLIMIT_NOFILE soft limit。
/// @param batch_id 本 control batch 的稳定唯一 identity。
/// @param count 本 batch 的全部 descriptor 数，包含非 AF_UNIX file。
/// @param sources 按 node id 排序的 AF_UNIX source nodes，保留重复边。
/// @param target transport commit 后拥有本 batch 的 receive endpoint。
/// @return graph edge 与 UID counter 已一起发布。
/// @errors graph/scratch reserve 失败返回 Socket(NoMemory)；quota 超限返回 NeedsCollection，
/// caller 必须释放 transport lock 后调用 collect 并重试。
pub(super) fn try_attach(
    uid: u32,
    limit: usize,
    batch_id: u64,
    count: usize,
    sources: &[UnixNode],
    target: &Arc<UnixSocket>,
) -> Result<(), AttachError> {
    let mut state = graph().lock();
    if state
        .inflight(uid)
        .checked_add(count)
        .is_none_or(|sum| sum > limit)
    {
        return Err(AttachError::NeedsCollection);
    }
    state
        .attach(uid, batch_id, count, sources, target)
        .map_err(AttachError::Socket)
}

/// @description 在任何 transport lock 外回收不可达 AF_UNIX rights cycle。
/// @param uid 待取得 quota 的 sender real UID。
/// @param limit sender 当前 RLIMIT_NOFILE soft limit。
/// @param count 下一 attach 将增加的全部 descriptor 数。
/// @return 已有并发 detach 释放 quota，或至少一个 dead SCC 已被 revoke。
/// @errors 没有可回收 SCC 且 quota 仍超限时返回 TooManyReferences。
pub(super) fn collect(uid: u32, limit: usize, count: usize) -> Result<(), SocketError> {
    let _collector = COLLECTOR.lock();
    let mut state = graph().lock();
    let before = state.inflight(uid);
    if state
        .inflight(uid)
        .checked_add(count)
        .is_some_and(|sum| sum <= limit)
    {
        return Ok(());
    }
    state.collect_cycles();
    let found = !state.reap.is_empty();
    drop(state);
    while let Some(socket) = graph().lock().reap.pop() {
        socket.revoke_rights();
    }
    if !found {
        return Err(SocketError::TooManyReferences);
    }
    let after = graph().lock().inflight(uid);
    // 只把当前 UID inflight 的严格下降视为 recovery progress。其他 UID 的 cycle 或
    // 已失效 reap candidate 不能驱动无界 retry，否则 quota pressure 会退化成 CPU 自旋。
    (after < before)
        .then_some(())
        .ok_or(SocketError::TooManyReferences)
}

/// @description 从 graph 与 UID counter 无分配摘除一个已消费/关闭 batch。
/// @param uid attach 时记录的 sender real UID。
/// @param batch_id 待摘除 control batch 的稳定 identity。
/// @param count attach 时计入 UID inflight 的全部 descriptor 数。
/// @param sources 按 node id 排序的 AF_UNIX source nodes。
/// @param target attach 时记录的 receiver node identity。
/// @return 无返回值；不存在或计数下溢表示内部 lifecycle 损坏并 fail-stop。
pub(super) fn detach(uid: u32, batch_id: u64, count: usize, sources: &[UnixNode], target: u64) {
    graph().lock().detach(uid, batch_id, count, sources, target);
}
