//! Pipe open 的完整端点 preparation、同锁 peer admission 与 prior counter snapshot。
use super::{Pipe, PipeDirection, PipeEnd, PipeOpenError, PipeSession, publish_state_change};
use alloc::sync::Arc;

/// 一次 open 所需方向；named nonblocking writer 可要求 live reader。
#[derive(Clone, Copy)]
pub(crate) enum PipeOpenAccess {
    Read,
    Write { require_reader: bool },
    ReadWrite,
}
/// 与 admission 同一次 state transaction 取得的 peer/counter 事实。
pub(crate) struct PipeOpenSnapshot {
    pub(crate) readers: usize,
    pub(crate) writers: usize,
    pub(crate) reader_opens: u64,
    pub(crate) writer_opens: u64,
}
/// 一次完整 open 的端点与之前的 peer 状态；不是 Pipe live state 的副本。
pub(crate) struct PipeOpened {
    pub(crate) reader: Option<Arc<PipeEnd>>,
    pub(crate) writer: Option<Arc<PipeEnd>>,
    pub(crate) prior: PipeOpenSnapshot,
}

/// 锁外已分配但未登记的端点；仅 admission 成功才允许其 Drop 更新计数。
pub(super) struct PreparedPipeEnd(pub(super) Arc<PipeEnd>);
impl PreparedPipeEnd {
    #[cfg(test)]
    pub(super) fn admit(self) -> Result<Arc<PipeEnd>, PipeOpenError> {
        let pipe = self.0.pipe.clone();
        let mut pending = [Some(self)];
        Self::admit_all(&pipe, &mut pending, false)?;
        Ok(pending[0].take().expect("prepared single endpoint").0)
    }

    // 多方向的一次 open 同锁发布；第二个端点 OOM 不得让对端看见半次 RW open。
    pub(super) fn admit_all(
        pipe: &Arc<Pipe>,
        pending: &mut [Option<Self>],
        require_reader: bool,
    ) -> Result<PipeOpenSnapshot, PipeOpenError> {
        let prior = {
            let mut state = pipe.state.lock();
            if state.session == PipeSession::Retired {
                return Err(PipeOpenError::Retired);
            }
            if require_reader && state.readers == 0 {
                return Err(PipeOpenError::NoReader);
            }
            let prior = PipeOpenSnapshot {
                readers: state.readers,
                writers: state.writers,
                reader_opens: state.reader_opens,
                writer_opens: state.writer_opens,
            };
            for pending in pending.iter_mut().flatten() {
                let end = Arc::get_mut(&mut pending.0).expect("unpublished endpoint is unique");
                assert!(core::ptr::eq(pipe.as_ref(), end.pipe.as_ref()));
                match end.direction {
                    PipeDirection::Read => {
                        state.readers += 1;
                        state.reader_opens += 1;
                        state.write_generation =
                            crate::execution::sync::next_readiness_generation();
                    }
                    PipeDirection::Write => {
                        state.writers += 1;
                        state.writer_opens += 1;
                        state.read_generation = crate::execution::sync::next_readiness_generation();
                    }
                }
                *end.registered.get_mut() = true;
            }
            state.session = PipeSession::Live;
            prior
        };
        publish_state_change(pipe);
        Ok(prior)
    }
}
