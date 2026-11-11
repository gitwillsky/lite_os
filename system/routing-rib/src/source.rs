//! 授权来源会话的寿命与完整替换；撤回只由主 Port 驱动，不能在 Drop 中分配或发 RPC。
use super::*;
use network_protocol::routing::{Grant, SourcePreparation, SourceService};
use router_protocol::tracking::installation::{self, Installation};

pub(super) struct Registration {
    pub(super) id: u64,
    pub(super) retiring: bool,
}
struct Session {
    owner: Arc<Rib>,
    id: u64,
    grant: Grant,
}
impl Rib {
    pub(super) fn open_source(self: &Arc<Self>, grant: Grant) -> Result<Channel, Status> {
        if grant.minimum_preference == 0 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let (client, server) = Channel::create()?;
        let id = {
            let mut state = self.state.lock();
            // 候选持有身份分配水位；租约期间不能创建未来会被旧候选覆盖的来源身份。
            if state.pending.is_some() {
                return Err(native_abi::ERR_SHOULD_WAIT);
            }
            state
                .sources
                .try_reserve(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            let id = state.routes.reserve_source()?;
            state.sources.push(Registration {
                id,
                retiring: false,
            });
            id
        };
        // 登记不保活 Session，避免 owner -> Session -> owner 环；失败析构与正常 peer-close 共用撤回。
        let session = Arc::try_new(Session {
            owner: self.clone(),
            id,
            grant,
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        service_rpc::serve(
            server,
            1,
            b"rib-source",
            Arc::try_new(
                move |ordinal,
                      request: &mut service_rpc::Reader<'_>,
                      handles,
                      reply: &mut service_rpc::Writer| {
                    network_protocol::routing::dispatch_source(
                        &*session, ordinal, request, handles, reply,
                    )
                },
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?,
        )?;
        Ok(client)
    }
}
impl SourceService for Session {
    fn watch_routes(&self) -> Result<(Channel, Event), Status> {
        self.owner.watch_routes(self.id)
    }
    fn track(
        &self,
        queries: Vec<router_protocol::tracking::Query>,
    ) -> Result<(Channel, Event), Status> {
        self.owner.track(queries, Some(self.id))
    }
    fn prepare(
        &self,
        batch: router_protocol::routing::Batch<'_>,
        queries: Vec<router_protocol::tracking::Query>,
        checks: Vec<Installation>,
    ) -> Result<SourcePreparation, Status> {
        if batch.family() == router_protocol::routing::Family::Ipv6 && !self.grant.ipv6_unicast {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        if queries.iter().any(|query| !query.valid()) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let source = Routes::capture_source(
            self.id,
            self.id,
            self.grant.protocol,
            self.grant.minimum_preference,
            batch,
        )?;
        let (client, server) = Channel::create()?;
        let mut state = self.owner.state.lock();
        if state.pending.is_some() {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        if !state
            .sources
            .iter()
            .any(|source| source.id == self.id && !source.retiring)
        {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let lease = state
            .next
            .checked_add(1)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let PreparedRoutes {
            next,
            encoded,
            outcomes,
            recovery_changed,
        } = state
            .routes
            .prepare_source(&self.owner.facts()?, source, &checks)?;
        drop(checks);
        // 查询与将要安装的 FIB 来自同一候选图；正文和 VMO 在取得执行租约/发表 pending 前准备。
        let candidate = next.snapshot().ok_or(native_abi::ERR_BAD_STATE)?;
        let bytes = candidate.encode(&queries)?;
        let length = bytes.len() as u64;
        let snapshot = native_runtime::Vmo::from_bytes(&bytes)?;
        drop(bytes);
        let verdicts = installation::encode_outcomes(self.id, candidate.generation(), &outcomes)
            .map_err(|error| match error {
                router_protocol::tracking::Error::Invalid => native_abi::ERR_INVALID_ARGS,
                router_protocol::tracking::Error::NoMemory => native_abi::ERR_NO_MEMORY,
            })?;
        drop(outcomes);
        let outcomes = (
            native_runtime::Vmo::from_bytes(&verdicts)?,
            verdicts.len() as u64,
        );
        let execution = self.owner.installer.prepare_configuration(encoded, network_protocol::ExecutionPolicy::default())?;
        state.next = lease;
        state.pending = Some(lease);
        drop(state);
        transaction::Transaction::serve(
            self.owner.clone(),
            lease,
            Some(self.id),
            next,
            execution,
            server,
        )?;
        Ok(SourcePreparation {
            source: self.id,
            transaction: client,
            snapshot: (snapshot, length),
            outcomes,
            recovery_changed,
        })
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock();
        if let Some(source) = state.sources.iter_mut().find(|source| source.id == self.id) {
            source.retiring = true;
            drop(state);
            self.owner
                .source_changes
                .signal(0, signals::USER_0)
                .expect("source retirement event remains valid");
        }
    }
}
