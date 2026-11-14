use alloc::sync::Arc;
use core::net::IpAddr;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::Mutex;

use super::{InetEndpoint, NetworkStack, SOCKET_STORAGE_CAPACITY};
use crate::sync::{TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation};
use device_model::network::{ReceivePacket, ReceivePreparation};
use network_protocol::SocketError;
use prefix_index::ReadLayout;

mod pending_cleanup;
use pending_cleanup::PendingCleanup;

const NETWORK_CLEANUP_BUDGET: usize = 64;
pub(super) const RECEIVE_PREFIX: usize = 0;
pub(super) const RECEIVE_FLOW_DOMAIN: usize = 1;
pub(super) const RECEIVE_FLOW_HASH: usize = 2;
pub(super) const RECEIVE_FLOW_OBSERVATION: core::ops::Range<usize> = 3..6;
// 无连接状态的裁决复用连接提示空间。首字为零时不是连接提示。
pub(super) const RECEIVE_FIREWALL: core::ops::Range<usize> = 1..6;
pub(super) const RECEIVE_SOURCE_PREFIX: usize = 6;
pub(super) const RECEIVE_TRANSIT_RULES: core::ops::Range<usize> = 7..9;
pub(super) const RECEIVE_FORWARD_HEADER: usize = 9;

type ReceiveIdentity = (u32, u32, [super::firewall::Inspection; 2], [bool; 2], u64, u64);

/// 预查选路使用的标记。已知防火墙丢弃时不查规则，缺少防火墙裁决时暂按零标记准备。
pub(super) fn receive_mark(annotation: &[u64; 10]) -> Option<u32> {
    match &annotation[RECEIVE_FIREWALL] {
        [0, generation, _, _, outcome] if *generation != 0 => {
            outcome.checked_sub(1).map(|mark| mark as u32)
        }
        _ => Some(0),
    }
}

struct NetworkStackState {
    stack: NetworkStack,
    // OWNER: 每个成功 take 在借出对象的空占位可见前后精确 +1/-1；缺失时 poll 会把 closed
    // placeholder 当真实 endpoint 推进并丢失 ingress/egress protocol state 或原生 UDP 队列。
    payload_loans: usize,
}

/// 同一域的结构布局和接收检查需求，供队列线程借用。
#[derive(Clone)]
pub(crate) struct ReceiveSnapshot {
    v4: Option<ReadLayout<u32>>,
    v6: Option<ReadLayout<u128>>,
    transport: [super::firewall::Inspection; 2],
    firewall: super::firewall::PolicySnapshot,
    selection: super::route::SelectionRules,
    domain: u64,
    flows: super::forward::ReceiveFlows,
}

impl ReceiveSnapshot {
    fn capture(stack: &NetworkStack) -> Self {
        let transport = Self::transport(stack);
        Self {
            v4: stack.routing.prefix_index().read_layout(),
            v6: stack.ipv6_routing.prefix_index().read_layout(),
            transport,
            firewall: transport
                .iter()
                .any(|inspection| !inspection.connections)
                .then(|| stack.firewall.snapshot())
                .flatten(),
            selection: if stack.selection_rules.transit_generation() == 0 {
                super::route::SelectionRules::default()
            } else {
                stack.selection_rules.clone()
            },
            domain: stack.domain,
            flows: stack.forwarding.receive_flows(),
        }
    }

    fn transport(stack: &NetworkStack) -> [super::firewall::Inspection; 2] {
        [false, true].map(|ipv6| stack.forwarding.inspection(&stack.firewall, ipv6))
    }

    fn identity(stack: &NetworkStack) -> ReceiveIdentity {
        (
            stack.routing.prefix_index().layout_stamp(),
            stack.ipv6_routing.prefix_index().layout_stamp(),
            Self::transport(stack),
            stack.forwarding.receive_udp(),
            stack.firewall.generation(),
            stack.selection_rules.transit_generation(),
        )
    }

    pub(super) fn prepare_for(&self, destination: IpAddr) -> ReceivePreparation {
        let annotation = match destination {
            IpAddr::V4(address) => self
                .v4
                .as_ref()
                .map_or(0, |index| index.probe(address.into())),
            IpAddr::V6(address) => self
                .v6
                .as_ref()
                .map_or(0, |index| index.probe(address.into())),
        };
        ReceivePreparation {
            annotation: [annotation, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            verify_transport: self.transport[usize::from(destination.is_ipv6())].required(),
        }
    }

    /// 为同一帧生成前缀、连接查找或防火墙提示。零入接口尚未发表，不能预判防火墙。
    pub(crate) fn prepare(
        &self,
        packet: ReceivePacket<'_>,
        separate_worker: bool,
        ingress: u32,
    ) -> ReceivePreparation {
        let Some(ip) = packet.ingress_ip() else {
            return ReceivePreparation::default();
        };
        let destination = ip.destination();
        let mut preparation = self.prepare_for(destination);
        if separate_worker
            && let Some((source, _)) = ip.addresses()
            && let Some(layout) = &self.v4
        {
            preparation.annotation[RECEIVE_SOURCE_PREFIX] = layout.probe(source.into());
        }
        let inspection = self.transport[usize::from(destination.is_ipv6())];
        if separate_worker {
            preparation.annotation[RECEIVE_FORWARD_HEADER] =
                super::forward::ReceiveHeader::prepare(packet, inspection.required());
        }
        let source = || match packet {
            ReceivePacket::Ipv4(ip) => ip.source().into(),
            ReceivePacket::Ipv6(ip) => ip.source().into(),
            _ => unreachable!("direct IP"),
        };
        if inspection.connections
            && let Some(flow) = self.flows.probe(packet, separate_worker)
        {
            preparation.annotation[RECEIVE_FLOW_DOMAIN] = self.domain;
            preparation.annotation[RECEIVE_FLOW_HASH] = flow.hash;
            preparation.annotation[RECEIVE_FLOW_OBSERVATION].copy_from_slice(&flow.observation);
        } else if separate_worker
            && ingress != 0
            && !inspection.connections
            && let Some(firewall) = &self.firewall
            && let Some(transport) = if inspection.transport {
                super::forward::prepare_transport(packet).map(Some)
            } else {
                Some(None)
            }
            && let Some(prepared) = firewall.prepare(ingress, (source(), destination), transport)
        {
            preparation.annotation[RECEIVE_FIREWALL].copy_from_slice(&prepared);
        }
        if separate_worker
            && ingress != 0
            && self.selection.transit_generation() != 0
            && let Some(mark) = receive_mark(&preparation.annotation)
        {
            let prepared = self.selection.prepare_transit(source(), ingress, mark);
            preparation.annotation[RECEIVE_TRANSIT_RULES].copy_from_slice(&prepared);
        }
        preparation
    }
}

/// 唯一 NetworkStack guard；task path 可睡眠取得，deferred observer 只能 `try_lock`。
pub(super) struct NetworkStackGuard<'a> {
    state: TaskMutexGuard<'a, NetworkStackState>,
    link_pending: &'a AtomicBool,
    receive_snapshot: &'a Mutex<ReceiveSnapshot>,
    original_receive: ReceiveIdentity,
    // 本 guard 取得 owner 后固定的 bounded-drain 结果；不复制持久 cleanup state。
    cleanup_backlog: bool,
}

impl Deref for NetworkStackGuard<'_> {
    type Target = NetworkStack;

    fn deref(&self) -> &Self::Target {
        &self.state.stack
    }
}

impl DerefMut for NetworkStackGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state.stack
    }
}

impl NetworkStackGuard<'_> {
    /// 在独占协议推进时消费通知。先清位再取样，期间到达的新通知留给下一轮。
    pub(super) fn poll_until(
        &mut self,
        timestamp: ::tcp::time::Instant,
        work_until: u64,
    ) -> super::poll::NetworkPoll {
        let refresh_links = self.link_pending.load(Ordering::Acquire)
            && self.link_pending.swap(false, Ordering::AcqRel);
        self.state.stack.poll_until(timestamp, work_until, refresh_links)
    }

    /// 返回本轮 fixed-budget final cleanup 后是否仍有 backlog。
    ///
    /// # Returns
    ///
    /// 尚有 cleanup identity 时为 true；caller 必须回投 Network bit。
    pub(super) const fn cleanup_backlog(&self) -> bool {
        self.cleanup_backlog
    }
}

impl Drop for NetworkStackGuard<'_> {
    fn drop(&mut self) {
        if ReceiveSnapshot::identity(&self.state.stack) != self.original_receive {
            let snapshot = ReceiveSnapshot::capture(&self.state.stack);
            let retired = core::mem::replace(&mut *self.receive_snapshot.lock(), snapshot);
            drop(retired);
        }
    }
}

/// 网络协议 state 的唯一 owner。
///
/// 普通 task 竞争时通过 `TaskMutex` 睡眠；deferred poll 只尝试取得 owner，竞争或存在
/// payload loan 时立即回投。payload loan 只在短 owner transaction 中用同类型空占位
/// 取出/归还库 socket 或原生 UDP 队列；大块 copy 不持全局互斥 guard，只保留 O(1) loan-count 完整性证明。
pub(crate) struct NetworkStackOwner {
    // OWNER: 唯一 protocol state；TaskMutex 只允许 task waiter 睡眠，deferred 只能 try。
    state: TaskMutex<NetworkStackState>,
    // OWNER: 驱动回调 Release 置位，协议 guard 先清位再读取设备。只合并通知，不保存载波。
    // 缺少该位会在普通收发轮次反复读链路，或在跳过读取时漏掉独立链路通知。
    link_pending: AtomicBool,
    // OWNER: 原域 guard 在布局或检查需求变化后发布，不复制 prefix/alias。批接收用 try_lock 短暂捕获，
    // 不持此锁遍历布局，也不取得域锁。缺少独立发布点会使繁忙域阻止队列并行查找。
    receive_snapshot: Mutex<ReceiveSnapshot>,
    // OWNER: final Drop 的 exactly-once commands。spin 临界区只有 O(1) publish/pop，绝不
    // 持锁执行 protocol cleanup，否则 deferred 与 drop 会恢复无界 busy wait。
    pending_cleanup: Mutex<PendingCleanup<InetEndpoint, SOCKET_STORAGE_CAPACITY>>,
}

impl NetworkStackOwner {
    /// 创建唯一 网络协议 owner 与空 final-cleanup ring。
    ///
    /// # Parameters
    ///
    /// - `stack`: 已完整装配且尚未发布的唯一 NetworkStack。
    ///
    /// # Returns
    ///
    /// 无第二份 state 的 owner。
    ///
    /// # Errors
    ///
    /// 不分配且无失败路径；零 cleanup capacity 由 const invariant fail-stop。
    pub(super) fn new(stack: NetworkStack) -> Self {
        Self {
            receive_snapshot: Mutex::new(ReceiveSnapshot::capture(&stack)),
            link_pending: AtomicBool::new(false),
            state: TaskMutex::new(NetworkStackState {
                stack,
                payload_loans: 0,
            }),
            pending_cleanup: Mutex::new(PendingCleanup::new()),
        }
    }

    /// 准备独立链路通知。回调只弱持协议 owner，设备退出后不会保活路由域。
    /// # Errors
    /// 回调分配失败时返回 NoMemory。
    pub(crate) fn link_activity(
        self: &Arc<Self>,
        domain: u64,
        observer: impl Fn() + Send + Sync + 'static,
    ) -> Result<device_model::Activity, SocketError> {
        let owner = Arc::downgrade(self);
        let activity: device_model::Activity = Arc::try_new(move || {
            let Some(owner) = owner.upgrade() else {
                return;
            };
            owner.link_pending.store(true, Ordering::Release);
            crate::worker::request_poll(domain);
            observer();
        })
        .map_err(|_| SocketError::NoMemory)?;
        Ok(activity)
    }

    /// 非阻塞借用本批的布局及校验需求。与发布竞争时由域线程按当前状态处理。
    pub(crate) fn receive_snapshot(&self) -> Option<ReceiveSnapshot> {
        self.receive_snapshot
            .try_lock()
            .map(|layouts| layouts.clone())
    }

    fn guard<'a>(
        &'a self,
        state: TaskMutexGuard<'a, NetworkStackState>,
        cleanup_backlog: bool,
    ) -> NetworkStackGuard<'a> {
        let original_receive = ReceiveSnapshot::identity(&state.stack);
        NetworkStackGuard {
            state,
            link_pending: &self.link_pending,
            receive_snapshot: &self.receive_snapshot,
            original_receive,
            cleanup_backlog,
        }
    }

    /// 可睡眠取得唯一 NetworkStack owner。
    ///
    /// # Returns
    ///
    /// 完整协议状态 guard。
    ///
    /// # Errors
    ///
    /// waiter metadata 分配失败时返回 `NoMemory`。
    pub(super) fn lock(&self) -> Result<NetworkStackGuard<'_>, SocketError> {
        let state = self.state.lock().map_err(|_| SocketError::NoMemory)?;
        Ok(self.consume_completed_closes(state))
    }

    /// 预备好的配置回滚使用同一 owner 与 close 清理；即使竞争也不再申请 waiter。
    pub(super) fn lock_prepared(
        &self,
        wait: &mut TaskMutexWaitPreparation,
    ) -> NetworkStackGuard<'_> {
        self.consume_completed_closes(self.state.lock_prepared(wait))
    }

    fn consume_completed_closes<'a>(
        &'a self,
        mut state: TaskMutexGuard<'a, NetworkStackState>,
    ) -> NetworkStackGuard<'a> {
        // 新 task transaction 必须先消费已完成 fput 的 commands；否则 close 返回后的
        // bind/new/read 会仍看到旧 port lease，或误用尚未清理的 retired slot。
        // state guard 阻止新 endpoint admission；每个已存在 registration 只发布一次 command。
        loop {
            let endpoint = self.pending_cleanup.lock().pop();
            let Some(endpoint) = endpoint else {
                break;
            };
            super::cleanup_endpoint(&mut state.stack, endpoint);
        }
        self.guard(state, false)
    }

    /// deferred poll 无等待地取得完整协议状态。
    ///
    /// # Returns
    ///
    /// owner 正忙或仍有 payload loan 时返回 `None`，由 caller 回投 Network bit。
    ///
    /// # Errors
    ///
    /// 无错误且不分配 waiter；竞争只返回 `None`。
    pub(super) fn try_poll(&self) -> Option<NetworkStackGuard<'_>> {
        let mut state = self.state.try_lock()?;
        // cleanup 可以先处理其他 endpoint；actual close 消费 operation 内的 registration，
        // 同一 guard 已等待全部 payload loan 恢复，observer Arc 不发布第二份 cleanup。
        let cleanup_backlog = self.drain_cleanup(&mut state.stack);
        if state.payload_loans != 0 {
            return None;
        }
        Some(self.guard(state, cleanup_backlog))
    }

    /// 在锁内只读协议自己的截止。竞争时由调用者重试，定时扫描不消费待清理状态。
    pub(super) fn poll_deadline(
        &self,
        now: ::tcp::time::Instant,
    ) -> Result<Option<::tcp::time::Instant>, ()> {
        let state = self.state.try_lock().ok_or(())?;
        if state.payload_loans != 0 {
            return Err(());
        }
        if self.pending_cleanup.lock().has_pending() {
            return Ok(Some(now));
        }
        Ok(state.stack.next_deadline(now))
    }

    /// 在 stack owner 外执行一个 endpoint-local payload 操作。
    ///
    /// # Parameters
    ///
    /// - `take`: 用同类型空占位从端点取出实际载荷队列。
    /// - `operation`: 只访问借出的 socket并执行 payload copy。
    /// - `restore`: 把真实传输对象归还原登记，并消费 placeholder。
    ///
    /// # Returns
    ///
    /// operation 的结果。
    ///
    /// # Errors
    ///
    /// owner waiter 预分配、take 或 operation 失败时返回 socket error；restore 不分配。
    pub(super) fn with_payload_loan<T, R>(
        &self,
        take: impl FnOnce(&mut NetworkStack) -> Result<T, SocketError>,
        operation: impl FnOnce(&mut T) -> Result<R, SocketError>,
        restore: impl FnOnce(&mut NetworkStack, T),
    ) -> Result<R, SocketError> {
        // restore 发生在 loan 已发布之后，必须在 take 前消除 waiter OOM 路径。
        let mut wait = TaskMutexWaitPreparation::prepare().map_err(|_| SocketError::NoMemory)?;
        let mut state = self.lock()?;
        let mut payload = take(&mut state)?;
        state.state.payload_loans = state
            .state
            .payload_loans
            .checked_add(1)
            .expect("network payload loan count overflow");
        drop(state);

        let result = operation(&mut payload);

        // poll 观察到 payload_loans 后只回投，不会持 owner 等待本 loan；prepared waiter
        // 仅覆盖与其他短 task transaction 的竞争。
        let mut state = self.guard(self.state.lock_prepared(&mut wait), false);
        restore(&mut state, payload);
        state.state.payload_loans = state
            .state
            .payload_loans
            .checked_sub(1)
            .expect("network payload loan restored without membership");
        result
    }

    /// final InetSocket drop 无等待清理；owner 正忙时把固定 identity 交给下一轮 poll。
    ///
    /// # Parameters
    ///
    /// - `endpoint`: exactly-once final Drop 保有且尚未释放 slot 的 identity。
    ///
    /// # Returns
    ///
    /// 无返回值；deferred publication 会同时回投 Network bit。
    ///
    /// # Errors
    ///
    /// 超过统一 socket admission capacity 表示 lifetime 不变量破坏并 fail-stop，不设 overflow fallback。
    pub(super) fn cleanup_or_defer(&self, domain: u64, endpoint: InetEndpoint) {
        if let Some(mut state) = self.state.try_lock() {
            super::cleanup_endpoint(&mut state.stack, endpoint);
            drop(state);
            crate::worker::request_poll(domain);
            return;
        }
        let mut pending = self.pending_cleanup.lock();
        pending
            .publish(endpoint)
            .expect("final network cleanup identity exceeds unique socket capacity");
        drop(pending);
        crate::worker::request_poll(domain);
    }

    fn drain_cleanup(&self, stack: &mut NetworkStack) -> bool {
        for _ in 0..NETWORK_CLEANUP_BUDGET {
            let endpoint = self.pending_cleanup.lock().pop();
            let Some(endpoint) = endpoint else {
                return false;
            };
            super::cleanup_endpoint(stack, endpoint);
        }
        self.pending_cleanup.lock().has_pending()
    }
}
