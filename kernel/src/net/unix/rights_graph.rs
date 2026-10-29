use alloc::{sync::Arc, vec::Vec};
use spin::{Mutex, Once};

use crate::{
    execution::sync::TaskMutex,
    fallible_tree::{FallibleMap, VacantEntry},
    net::SocketError,
};

use super::{UnixNode, UnixPassedFile, UnixSocket};

type EdgeKey = (u64, usize);

/// 图边直接拥有 successor identity；backlog 边表达实际未接受连接归属，不代表 OFD 引用。
struct GraphEdge {
    successor: Arc<GraphNode>,
    file_reference: bool,
}

struct NodeState {
    edges: FallibleMap<EdgeKey, GraphEdge>,
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
    next_edge: Option<EdgeKey>,
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
    // 实际 Weak probe pins 延后退休，避免 GRAPH 内的最后 OFD/Socket Drop 重入 detach。
    probes: Vec<Arc<UnixSocket>>,
    roots: Vec<Arc<dyn UnixPassedFile>>,
}

// OWNER: all attach/detach/topology mutations are serialized here. Socket queues never take GRAPH
// while GRAPH calls into a socket, preventing graph↔socket lock inversion.
static GRAPH: Once<Mutex<RightsGraph>> = Once::new();
// OWNER: only one attach transaction may release GRAPH to reap proven cycles and retry its limit
// check. Detach deliberately bypasses this gate so revocation can make progress synchronously.
static COLLECTOR: TaskMutex<()> = TaskMutex::new(());

/// transport lock 内 fast attach 的内部结果；不直接泄漏为 socket errno。
pub(super) enum AttachError {
    /// 当前 UID quota 需要在 transport lock 外执行 cycle collection。
    NeedsCollection,
    /// 并发 topology 增长超过已准备 scratch；caller 释放 transport guard 后重新准备。
    NeedsPreparation,
}

impl GraphNode {
    fn new(node: UnixNode) -> Result<Arc<Self>, SocketError> {
        Arc::try_new(Self {
            node,
            state: Mutex::new(NodeState {
                edges: FallibleMap::new(),
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
            probes: Vec::new(),
            roots: Vec::new(),
        }
    }

    fn inflight(&self, uid: u32) -> usize {
        self.uid_inflight.get(&uid).copied().unwrap_or(0)
    }

    fn scratch_capacity(&self) -> usize {
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
    fn take_scratch(&mut self) -> GraphScratch {
        GraphScratch {
            scan: core::mem::take(&mut self.scan),
            dfs: core::mem::take(&mut self.dfs),
            stack: core::mem::take(&mut self.stack),
            members: core::mem::take(&mut self.members),
            components: core::mem::take(&mut self.components),
            reap: core::mem::take(&mut self.reap),
            probes: core::mem::take(&mut self.probes),
            roots: core::mem::take(&mut self.roots),
        }
    }
    fn install_scratch(&mut self, scratch: GraphScratch) -> GraphScratch {
        let old = self.take_scratch();
        self.scan = scratch.scan;
        self.dfs = scratch.dfs;
        self.stack = scratch.stack;
        self.members = scratch.members;
        self.components = scratch.components;
        self.reap = scratch.reap;
        self.probes = scratch.probes;
        self.roots = scratch.roots;
        old
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
    )]
    fn attach(
        &mut self,
        uid: Option<u32>,
        batch_id: u64,
        count: usize,
        sources: &[UnixNode],
        target: u64,
        limit: Option<usize>,
        prepared: &mut PreparedAttach,
    ) -> Result<(), AttachError> {
        // Linux too_many_unix_fds 判断已存在 inflight > soft limit；当前 batch 可越过边界。
        if limit
            .is_some_and(|limit| self.inflight(uid.expect("SCM quota requires real UID")) > limit)
        {
            return Err(AttachError::NeedsCollection);
        }
        let missing = prepared
            .nodes
            .iter()
            .filter(|node| !self.nodes.contains_key(node.key()))
            .count();
        let needed = self
            .nodes
            .len()
            .checked_add(missing)
            .expect("AF_UNIX graph size overflow");
        if self.scratch_capacity() < needed {
            if prepared
                .scratch
                .as_ref()
                .is_none_or(|scratch| scratch.capacity() < needed)
            {
                return Err(AttachError::NeedsPreparation);
            }
            let next = prepared.scratch.take().unwrap();
            prepared.scratch = Some(self.install_scratch(next));
        }
        // 所有检验已结束；预留节点一次无分配发布。未使用的 alternative 留在 token 中锁外退休。
        let mut index = 0;
        while index < prepared.nodes.len() {
            if self.nodes.contains_key(prepared.nodes[index].key()) {
                index += 1;
            } else {
                self.nodes.commit_vacant(prepared.nodes.swap_remove(index));
            }
        }
        if let Some(uid) = uid
            && !self.uid_inflight.contains_key(&uid)
        {
            self.uid_inflight
                .commit_vacant(prepared.uid.take().expect("prepared UID node"));
        }
        let target_node = (!sources.is_empty()).then(|| self.nodes.get(&target).unwrap());
        // edges 预留时按 descriptor ordinal 保留重复 source，reverse pop 不改变真实 topology。
        for (index, source) in sources.iter().enumerate().rev() {
            let node = self.nodes.get(&source.id).expect("actual AF_UNIX source");
            let mut state = node.state.lock();
            let edge = prepared.edges.pop().expect("prepared AF_UNIX edge").fill(
                (batch_id, index),
                GraphEdge {
                    successor: target_node.unwrap().clone(),
                    file_reference: uid.is_some(),
                },
            );
            state.edges.commit_vacant(edge);
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
                .expect("AF_UNIX incoming overflow");
            state.references = state
                .references
                .checked_add(sources.len())
                .expect("AF_UNIX graph reference overflow");
        }
        if let Some(uid) = uid {
            let inflight = self.uid_inflight.get_mut(&uid).unwrap();
            *inflight = inflight
                .checked_add(count)
                .expect("SCM_RIGHTS UID inflight overflow");
        }
        Ok(())
    }
    fn detach(
        &mut self,
        uid: Option<u32>,
        batch_id: u64,
        count: usize,
        sources: &[UnixNode],
        target: u64,
    ) -> RetiredGraph {
        let mut retired = RetiredGraph {
            edges: FallibleMap::new(),
            nodes: FallibleMap::new(),
            uid: None,
        };
        let mut removed = 0;
        for (index, source) in sources.iter().enumerate() {
            let node = self.nodes.get(&source.id).expect("attached source missing");
            let mut state = node.state.lock();
            let edge = state
                .edges
                .take_entry(&(batch_id, index))
                .expect("attached edge missing");
            retired.edges.commit_vacant(edge);
            state.references = state
                .references
                .checked_sub(1)
                .expect("AF_UNIX source reference underflow");
            removed += 1;
        }
        if removed != 0 {
            let node = self.nodes.get(&target).expect("attached receiver missing");
            let mut state = node.state.lock();
            state.incoming = state
                .incoming
                .checked_sub(removed)
                .expect("AF_UNIX target incoming underflow");
            state.references = state
                .references
                .checked_sub(removed)
                .expect("AF_UNIX target reference underflow");
        }
        if let Some(uid) = uid {
            let inflight = self
                .uid_inflight
                .get_mut(&uid)
                .expect("attached UID missing");
            *inflight = inflight
                .checked_sub(count)
                .expect("SCM_RIGHTS UID inflight underflow");
            if *inflight == 0 {
                retired.uid = self.uid_inflight.take_entry(&uid);
            }
        }
        let mut previous = None;
        for source in sources {
            if previous != Some(source.id) {
                self.remove_unreferenced(source.id, &mut retired.nodes);
            }
            previous = Some(source.id);
        }
        self.remove_unreferenced(target, &mut retired.nodes);
        retired
    }
    fn remove_unreferenced(&mut self, id: u64, retired: &mut FallibleMap<u64, Arc<GraphNode>>) {
        let remove = self.nodes.get(&id).is_some_and(|node| {
            let state = node.state.lock();
            assert_eq!(
                state.references,
                state
                    .incoming
                    .checked_add(state.edges.len())
                    .expect("AF_UNIX incident overflow")
            );
            state.references == 0
        });
        if remove {
            retired.commit_vacant(
                self.nodes
                    .take_entry(&id)
                    .expect("unreferenced AF_UNIX node"),
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
        self.dfs.push(DfsFrame {
            node,
            next_edge: None,
        });
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
                    let edge = match frame.next_edge {
                        Some(key) => state.edges.successor(&key),
                        None => state.edges.first_key_value(),
                    };
                    edge.map(|(key, successor)| {
                        frame.next_edge = Some(*key);
                        successor.successor.clone()
                    })
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
                let outgoing = node
                    .state
                    .lock()
                    .edges
                    .values()
                    .filter(|edge| edge.file_reference)
                    .count();
                if let Some(socket) = node.node.socket.upgrade() {
                    let probe = socket.root_probe(outgoing);
                    dead &= !probe.rooted;
                    if let Some(owner) = probe.owner {
                        self.roots.push(owner);
                    }
                    self.probes.push(socket);
                } else {
                    dead = false;
                }
                let state = node.state.lock();
                for edge in state.edges.values() {
                    let successor = &edge.successor;
                    let dependency = if Arc::ptr_eq(node, successor) {
                        state.component
                    } else {
                        successor.state.lock().component
                    };
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

mod preparation;
use preparation::GraphScratch;
pub(super) use preparation::PreparedAttach;
struct RetiredGraph {
    edges: FallibleMap<EdgeKey, GraphEdge>,
    nodes: FallibleMap<u64, Arc<GraphNode>>,
    uid: Option<VacantEntry<u32, usize>>,
}
/// 已准备 graph 的无分配发布；失败保留全部 token，要求 caller 锁外 recovery/reprepare。
pub(super) fn try_attach(
    uid: u32,
    batch_id: u64,
    count: usize,
    sources: &[UnixNode],
    target: u64,
    limit: Option<usize>,
    prepared: &mut PreparedAttach,
) -> Result<(), AttachError> {
    graph()
        .lock()
        .attach(Some(uid), batch_id, count, sources, target, limit, prepared)
}

/// 在任何 transport lock 外回收不可达 AF_UNIX rights cycle。
///
/// # Parameters
///
/// - `uid`: 待取得 quota 的 sender real UID。
/// - `limit`: sender 当前 RLIMIT_NOFILE soft limit。
///
/// # Returns
///
/// 已有并发 detach 释放 quota，或至少一个 dead SCC 已被 revoke。
///
/// # Errors
///
/// 没有可回收 SCC 且 quota 仍超限时返回 TooManyReferences。
pub(super) fn collect(uid: u32, limit: usize) -> Result<(), SocketError> {
    let _collector = COLLECTOR.lock().map_err(|_| SocketError::NoMemory)?;
    let state = graph().lock();
    let before = state.inflight(uid);
    if state.inflight(uid) <= limit {
        return Ok(());
    }
    let found = collect_pass(state);
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

// OWNER: merged GC request event；actual collector kernel thread 单独消费，不在 IRQ/deferred
// safe point 运行 Tarjan 或释放 OFD。无请求时 worker 无限休眠；有 live topology 时用已预留 deadline
// 复查最后 descriptor/active-syscall root 的退休，避免仅在 quota 超限时才释放孤立 cycle。
static COLLECTION_REQUEST: crate::execution::sync::TaskEvent =
    crate::execution::sync::TaskEvent::new();
pub(in crate::net::unix) fn request_collection() {
    COLLECTION_REQUEST.signal();
}
fn collect_pass(mut state: spin::MutexGuard<'_, RightsGraph>) -> bool {
    state.collect_cycles();
    let mut scratch = state.take_scratch();
    let found = !scratch.reap.is_empty();
    drop(state);
    while let Some(socket) = scratch.reap.pop() {
        socket.revoke_rights();
    }
    // 旧 scan/member pins 可能是已摘除 GraphNode 的最后引用；只在 GRAPH guard 外 clear。
    scratch.clear();
    let mut state = graph().lock();
    let retired = if state.scratch_capacity() < scratch.capacity() {
        state.install_scratch(scratch)
    } else {
        scratch
    };
    drop(state);
    drop(retired);
    found
}
/// 唯一实际 collector worker；clock/sleep 是 composition root 的 scheduler seam，所有 deadline storage
/// 在启动期预留，不让 socket 反向依赖 task。只有存在实际 topology 时才定期复查 root retirement。
pub(in crate::net) fn run_collector(
    clock: impl Fn() -> u64,
    mut sleep_until: impl FnMut(u64),
    mut gate: crate::execution::sync::TaskMutexWaitPreparation,
) -> ! {
    const ROOT_RECHECK_NS: u64 = 100_000_000;
    loop {
        COLLECTION_REQUEST.wait_idle();
        loop {
            {
                let _collector = COLLECTOR.lock_prepared(&mut gate);
                let state = graph().lock();
                if state.nodes.is_empty() {
                    break;
                }
                collect_pass(state);
            }
            if graph().lock().nodes.is_empty() {
                break;
            }
            sleep_until(clock().saturating_add(ROOT_RECHECK_NS));
        }
    }
}

/// 从 graph 与 UID counter 无分配摘除一个已消费/关闭 batch。
///
/// # Parameters
///
/// - `uid`: attach 时记录的 sender real UID。
/// - `batch_id`: 待摘除 control batch 的稳定 identity。
/// - `count`: attach 时计入 UID inflight 的全部 descriptor 数。
/// - `sources`: 按 node id 排序的 AF_UNIX source nodes。
/// - `target`: attach 时记录的 receiver node identity。
///
/// # Returns
///
/// 无返回值；不存在或计数下溢表示内部 lifecycle 损坏并 fail-stop。
pub(super) fn detach(uid: u32, batch_id: u64, count: usize, sources: &[UnixNode], target: u64) {
    let retired = graph()
        .lock()
        .detach(Some(uid), batch_id, count, sources, target);
    drop(retired);
    request_collection();
}

/// accept 归属转移与 collector root snapshot 共用 GRAPH 线性化点。
/// 实际旧 backlog capability 在 GRAPH/root guard 外退休，不在 guard 内重入 detach。
pub(super) fn publish_description(
    socket: &UnixSocket,
    owner: alloc::sync::Weak<dyn UnixPassedFile>,
) {
    let state = graph().lock();
    let retired = socket.rights_owner.lock().replace_description(owner);
    drop(state);
    drop(retired);
    request_collection();
}

/// 一个真实 backlog membership 的 graph capability；只在 endpoint 的 root owner 内存活。
/// 不计入 UID quota 或 OFD 引用数，Drop 精确移除该归属边，所有退休节点锁外释放。
pub(super) struct BacklogOwnership {
    server: UnixNode,
    listener: u64,
    edge_id: u64,
}
impl Drop for BacklogOwnership {
    fn drop(&mut self) {
        let retired = graph().lock().detach(
            None,
            self.edge_id,
            0,
            core::slice::from_ref(&self.server),
            self.listener,
        );
        drop(retired);
        request_collection();
    }
}

/// 未发布 backlog 归属边的锁外准备；不持 queue guard，不安装第二个 membership registry。
pub(super) struct PreparedBacklog {
    server: UnixNode,
    listener: u64,
    edge_id: u64,
    graph: PreparedAttach,
}
impl PreparedBacklog {
    /// 为真实 server→listener 归属准备节点、边和 collector scratch；OOM 无副作用。
    pub(super) fn new(
        server: &Arc<UnixSocket>,
        listener: &Arc<UnixSocket>,
    ) -> Result<Self, SocketError> {
        let server = server.node();
        let graph = PreparedAttach::new(None, core::slice::from_ref(&server), listener)?;
        Ok(Self {
            server,
            listener: listener.node_id(),
            edge_id: crate::id::next_runtime_object_id(),
            graph,
        })
    }
    /// 在 creator 尚不可观察时发布归属；并发图增长仅在 GRAPH 外补充 scratch。
    pub(super) fn publish(mut self) -> Result<BacklogOwnership, SocketError> {
        loop {
            let result = graph().lock().attach(
                None,
                self.edge_id,
                0,
                core::slice::from_ref(&self.server),
                self.listener,
                None,
                &mut self.graph,
            );
            match result {
                Ok(()) => {
                    return Ok(BacklogOwnership {
                        server: self.server,
                        listener: self.listener,
                        edge_id: self.edge_id,
                    });
                }
                Err(AttachError::NeedsPreparation) => self.graph.refresh_scratch()?,
                Err(AttachError::NeedsCollection) => {
                    unreachable!("backlog ownership has no UID quota")
                }
            }
        }
    }
}

/// namespace lookup 后捕获实际 listener file root；与整个收集/撤销 pass 串行。
/// 不允许在 collector 已判 dead 与真正撤销 controls 之间用历史 Weak 复活该 file。
/// # Errors
/// waiter OOM 或已关闭 listener；不得持 socket/transport guard 调用。
pub(super) fn pin_description(socket: &UnixSocket) -> Result<Arc<dyn UnixPassedFile>, SocketError> {
    let _collector = COLLECTOR.lock().map_err(|_| SocketError::NoMemory)?;
    match &*socket.rights_owner.lock() {
        super::rights_owner::RightsOwner::Description(owner) => {
            owner.upgrade().ok_or(SocketError::ConnectionRefused)
        }
        _ => Err(SocketError::ConnectionRefused),
    }
}
