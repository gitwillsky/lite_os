//! 授权来源会话的寿命与完整替换；撤回只由主 Port 驱动，不能在 Drop 中分配或发 RPC。
use super::*;
use network_protocol::routing::{Grant, SourceService};

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
    fn track(
        &self,
        queries: Vec<router_protocol::tracking::Query>,
    ) -> Result<(Channel, Event), Status> {
        self.owner.track(queries)
    }
    fn replace(&self, batch: router_protocol::routing::Batch<'_>) -> Result<(), Status> {
        let source = Routes::capture_source(
            self.id,
            self.id,
            self.grant.protocol,
            self.grant.minimum_preference,
            batch,
        )?;
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
        let PreparedRoutes { next, encoded } =
            state
                .routes
                .prepare_sources(&self.owner.facts()?, Some(source), &[])?;
        let execution = self.owner.installer.prepare_configuration(encoded)?;
        if execution.activate().is_err() {
            let _ = execution.abort();
            return Err(native_abi::ERR_BAD_STATE);
        }
        transaction::confirm_execution(execution);
        let old = state.publish(next);
        drop(state);
        drop(old);
        Ok(())
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
