//! Volatile inode 的唯一时间 tuple；storage/mmap 与 inode attributes 共享同一事实。
use super::FileSystemError;
use alloc::sync::Arc;
use spin::Mutex;

/// 一次锁下取得的真实 inode 时间快照；秒精度与当前 filesystem ABI profile 一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TimeSnapshot {
    pub(super) atime: u64,
    pub(super) mtime: u64,
    pub(super) ctime: u64,
}
impl TimeSnapshot {
    fn new() -> Self {
        let now = now_seconds();
        Self {
            atime: now,
            mtime: now,
            ctime: now,
        }
    }
    fn touch(&mut self, now: u64) {
        self.mtime = now;
        self.ctime = now;
    }
}

/// MemoryFile 与 resident-page mmap writer 共用的时间 owner。
///
/// 锁只持三个 scalar，guard 内不分配、不等待、不调用 inode/MemoryFile 或 timer owner。
/// 时间先在锁外取得；MM touch 不反入 metadata，锁序只能 metadata→times。
pub(super) struct InodeTimes(Mutex<TimeSnapshot>);
impl InodeTimes {
    pub(super) fn new() -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self(Mutex::new(TimeSnapshot::new())))
            .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(super) fn snapshot(&self) -> TimeSnapshot {
        *self.0.lock()
    }
    pub(super) fn touch(&self) {
        let now = now_seconds();
        self.0.lock().touch(now);
    }
}

/// 非 content inode 的 inline tuple，或实际 MemoryFile tuple 的保活引用；不复制文件时间。
pub(super) enum TimeOwner {
    Inline(TimeSnapshot),
    Memory(Arc<InodeTimes>),
}
impl TimeOwner {
    pub(super) fn new() -> Self {
        Self::Inline(TimeSnapshot::new())
    }
    pub(super) fn snapshot(&self) -> TimeSnapshot {
        match self {
            Self::Inline(times) => *times,
            Self::Memory(times) => times.snapshot(),
        }
    }
    pub(super) fn touch_at(&mut self, now: u64) {
        match self {
            Self::Inline(times) => times.touch(now),
            Self::Memory(times) => times.0.lock().touch(now),
        }
    }
    pub(super) fn change_at(&mut self, now: u64) {
        match self {
            Self::Inline(times) => times.ctime = now,
            Self::Memory(times) => times.0.lock().ctime = now,
        }
    }
    /// 普通读取只更改 atime，不更改 ctime/mtime；同一真实 tuple 内复查 relatime 条件。
    pub(super) fn access_at(&mut self, now: u64, relatime: bool) {
        let update = |times: &mut TimeSnapshot| {
            if needs_access_update(times.atime, times.mtime, times.ctime, now, relatime) {
                times.atime = now;
            }
        };
        match self {
            Self::Inline(times) => update(times),
            Self::Memory(times) => update(&mut times.0.lock()),
        }
    }
    pub(super) fn set(&mut self, atime: Option<u64>, mtime: Option<u64>) {
        // 不在 times 短锁内读取 timer；显式 mtime 可以是过去或未来，ctime 始终是本次变更时刻。
        self.set_at(now_seconds(), atime, mtime);
    }
    /// Caller 在任何 inode 短锁前读取 clock；本方法只更新已有时间 tuple。
    pub(super) fn set_at(&mut self, now: u64, atime: Option<u64>, mtime: Option<u64>) {
        let set = |times: &mut TimeSnapshot| {
            if atime.is_none() && mtime.is_none() {
                return;
            }
            if let Some(time) = atime {
                times.atime = time;
            }
            if let Some(time) = mtime {
                times.mtime = time;
            }
            times.ctime = now;
        };
        match self {
            Self::Inline(times) => set(times),
            Self::Memory(times) => set(&mut times.0.lock()),
        }
    }
}
fn now_seconds() -> u64 {
    crate::execution::timer::get_realtime_ns() / 1_000_000_000
}

/// 固定 Linux relatime_need_update/atime_needs_update；时钟倒退也可更新，不能用 max 冻结未来 atime。
pub(super) fn needs_access_update(
    atime: u64,
    mtime: u64,
    ctime: u64,
    now: u64,
    relatime: bool,
) -> bool {
    now != atime
        && (!relatime || atime <= mtime || atime <= ctime || now.saturating_sub(atime) >= 86_400)
}
