//! 后台预算与等待计数；队列/预算同属 Connection.queue，令牌只拥有 waiting 的一次计数。
use super::*;

/// fusectl 可调整的后台参数。
#[derive(Clone, Copy)]
pub(in crate::fs::fuse) enum Limit {
    Maximum,
    Congestion,
}

/// 固定 Linux 的默认非特权预算：managed RAM / 8192 / 392，封顶 65535。
pub(in crate::fs::fuse) fn unprivileged_limit() -> u16 {
    let bytes = crate::memory::frame_statistics().capacity_pages as u64 * PAGE_SIZE as u64;
    (bytes / 8192 / 392).min(u64::from(u16::MAX)) as u16
}

/// 一个实际请求的 waiting 计数令牌；Drop 归还计数，不持连接或承担同步。
pub(super) struct Waiting(Arc<AtomicUsize>);
impl Waiting {
    pub(super) fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter.clone())
    }
}
impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 唯一后台预算事实；所有字段仅在 Connection.queue 锁内读写，不建立影子 blocked/congested flag。
pub(super) struct Background {
    /// 未获预算的请求；active 只统计已移入 pending/processing、尚未完成的后台请求。
    pub(super) pending: VecDeque<Arc<Request>>,
    pub(super) active: usize,
    maximum: u16,
    congestion: u16,
}
impl Background {
    pub(super) fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            active: 0,
            maximum: 12,
            congestion: 9,
        }
    }
    pub(super) fn negotiate(&mut self, parameters: Negotiated, privileged: bool) {
        if parameters.minor < 13 {
            return;
        }
        let limit = if privileged {
            u16::MAX
        } else {
            unprivileged_limit()
        };
        if parameters.max_background != 0 {
            self.maximum = parameters.max_background.min(limit);
        }
        if parameters.congestion_threshold != 0 {
            self.congestion = parameters.congestion_threshold.min(limit);
        }
    }
}

impl Queue {
    /// reserve/前台 enqueue 预留了 deferred 项的 pending 槽；promotion 只移动 Arc，不分配。
    pub(super) fn promote_background(&mut self) {
        while self.background.active < usize::from(self.background.maximum) {
            let Some(request) = self.background.pending.pop_front() else {
                break;
            };
            self.background.active += 1;
            self.pending.push_back(request);
        }
    }
}

impl Connection {
    pub(super) fn enqueue_background(&self, request: Arc<Request>) -> Result<(), FileSystemError> {
        let mut queue = self.queue.lock();
        if let Some(error) = queue.refusal() {
            return Err(error);
        }
        // 1. 同时预留最终 pending 与 deferred 槽；之后的 finish/promotion/RELEASE Drop 均不分配。
        let needed = queue.reserved + queue.background.pending.len() + 1;
        queue
            .pending
            .try_reserve(needed)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let needed = queue.reserved + 1;
        queue
            .background
            .pending
            .try_reserve(needed)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        // 2. 由同一 budget owner 准入，包含初始化请求；前台调用独立，不被后台限流。
        queue.background.pending.push_back(request);
        queue.promote_background();
        drop(queue);
        self.readiness.1.signal_readiness();
        Ok(())
    }

    /// 完成请求仅归还一次后台预算；abort 已清空预算时不再扣减，所有 completion 路径共用此处。
    pub(super) fn finish(&self, request: &Request, result: Result<Vec<u8>, FileSystemError>) {
        let initialized = (request.opcode == fuse::INIT).then(|| {
            result
                .as_deref()
                .map_err(|error| *error)
                .and_then(Negotiated::parse)
        });
        if !request.finish(result) {
            return;
        }
        if let Some(initialized) = initialized {
            self.finish_init(initialized);
        }
        if !request.background {
            return;
        }
        let mut queue = self.queue.lock();
        if queue.link == Link::Connected {
            queue.background.active -= 1;
            queue.promote_background();
        }
        drop(queue);
        self.readiness.1.signal_readiness();
    }

    /// 返回实际请求令牌数，包括等 INIT 的调用，不含未发送的预留 RELEASE/FORGET。
    pub(in crate::fs::fuse) fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    /// 返回 `limit` 的当前值；无复制预算，读取同一 queue owner。
    pub(in crate::fs::fuse) fn limit(&self, limit: Limit) -> u16 {
        let queue = self.queue.lock();
        match limit {
            Limit::Maximum => queue.background.maximum,
            Limit::Congestion => queue.background.congestion,
        }
    }

    /// 设置已完成权限/范围检查的 `limit`/`value`；放宽预算后立即发布积压请求，无失败分支。
    pub(in crate::fs::fuse) fn set_limit(&self, limit: Limit, value: u16) {
        let mut queue = self.queue.lock();
        match limit {
            Limit::Maximum => queue.background.maximum = value,
            Limit::Congestion => queue.background.congestion = value,
        }
        queue.promote_background();
        drop(queue);
        self.readiness.1.signal_readiness();
    }

    /// 积压与活动后台请求达到 congestion threshold 时，调用方只填充 demand 页，停止额外预读。
    pub(in crate::fs::fuse) fn congested(&self) -> bool {
        let queue = self.queue.lock();
        queue.background.active + queue.background.pending.len()
            >= usize::from(queue.background.congestion)
    }
}
