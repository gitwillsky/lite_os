//! 原生依赖观察的寿命与发表通知；只保留观察定义，不镜像解析结果或转发状态。
use super::*;
use network_protocol::routing::TrackingService;
use router_protocol::tracking::Query;

pub(super) struct Registration {
    id: u64,
    queries: Arc<Vec<Query>>,
    changed: Event,
}
struct Session {
    owner: Arc<Rib>,
    id: u64,
}
impl State {
    // 仅在 NET 确认后调用；比较不分配，kernel Event 不反向同步调用 RIB，通知失败不能留下半个发表。
    pub(super) fn publish(&mut self, next: Routes) -> Routes {
        let old = core::mem::replace(&mut self.routes, next);
        for watcher in &self.watchers {
            let changed = match (old.snapshot(), self.routes.snapshot()) {
                (Some(old), Some(next)) => next.changes_from(old, &watcher.queries),
                (None, None) => false,
                _ => true,
            };
            if changed {
                watcher
                    .changed
                    .signal(0, signals::USER_0)
                    .expect("registered watch event remains valid");
            }
        }
        old
    }
}
impl Rib {
    pub(super) fn track(self: &Arc<Self>, queries: Vec<Query>) -> Result<(Channel, Event), Status> {
        if queries.iter().any(|query| !query.valid()) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        // 1. 完整定义/返回能力先准备；登记不持 Session 强引用，避免 owner -> Session -> owner 环。
        let queries = Arc::try_new(queries).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let changed = Event::create()?;
        let returned = changed.duplicate(native_abi::rights::SAME)?;
        let (client, server) = Channel::create()?;
        changed.signal(0, signals::USER_0)?;
        let id = {
            let mut state = self.state.lock();
            state
                .watchers
                .try_reserve(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            let id = state
                .next
                .checked_add(1)
                .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
            state.next = id;
            state.watchers.push(Registration {
                id,
                queries,
                changed,
            });
            id
        };
        // 2. guard 在 state 锁外创建/退休；启动失败也沿正常会话关闭路径移除登记，不发 RPC。
        let session = Arc::try_new(Session {
            owner: self.clone(),
            id,
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        service_rpc::serve(
            server,
            1,
            b"rib-tracking",
            Arc::try_new(
                move |ordinal,
                      request: &mut service_rpc::Reader<'_>,
                      handles,
                      reply: &mut service_rpc::Writer| {
                    network_protocol::routing::dispatch_tracking(
                        &*session, ordinal, request, handles, reply,
                    )
                },
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?,
        )?;
        Ok((client, returned))
    }
}
impl TrackingService for Session {
    fn snapshot(&self) -> Result<Vec<u8>, Status> {
        let (queries, snapshot) = {
            let state = self.owner.state.lock();
            let watcher = state
                .watchers
                .iter()
                .find(|watcher| watcher.id == self.id)
                .ok_or(native_abi::ERR_PEER_CLOSED)?;
            let snapshot = state
                .routes
                .snapshot()
                .cloned()
                .ok_or(native_abi::ERR_SHOULD_WAIT)?;
            (watcher.queries.clone(), snapshot)
        };
        // 同一锁下捕获定义/代际，编码不占 RIB 锁；Arc 保活同一份不可变图，没有行/成员副本。
        snapshot.encode(&queries)
    }
    fn update(&self, queries: Vec<Query>) -> Result<(), Status> {
        if queries.iter().any(|query| !query.valid()) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let queries = Arc::try_new(queries).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut state = self.owner.state.lock();
        let watcher = state
            .watchers
            .iter_mut()
            .find(|watcher| watcher.id == self.id)
            .ok_or(native_abi::ERR_PEER_CLOSED)?;
        if watcher.queries == queries {
            return Ok(());
        }
        let old = core::mem::replace(&mut watcher.queries, queries);
        watcher
            .changed
            .signal(0, signals::USER_0)
            .expect("registered watch event remains valid");
        drop(state);
        drop(old);
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let removed = {
            let mut state = self.owner.state.lock();
            state
                .watchers
                .iter()
                .position(|watcher| watcher.id == self.id)
                .map(|position| state.watchers.swap_remove(position))
        };
        // 查询大数组和 kernel 能力在 RIB 锁外退休，不需分配关闭消息或等待其他服务。
        drop(removed);
    }
}
