use alloc::sync::Arc;

use super::{Ext4FileSystem, FileSystemError, Journal, RunningTransaction, StagedBlocks};

/// Journal runtime 的唯一状态；commit loan 期间只发布不可变 staged view。
pub(in crate::fs::ext4) enum JournalOwner {
    Unavailable,
    Ready(Journal),
    Committing {
        running: Arc<StagedBlocks>,
        active: Option<Arc<StagedBlocks>>,
    },
}

impl JournalOwner {
    pub(in crate::fs::ext4) const fn unavailable() -> Self {
        Self::Unavailable
    }

    pub(in crate::fs::ext4) fn install(&mut self, journal: Journal) {
        assert!(matches!(self, Self::Unavailable));
        *self = Self::Ready(journal);
    }

    pub(in crate::fs::ext4) fn ready_mut_ref(&self) -> Result<&Journal, FileSystemError> {
        match self {
            Self::Ready(journal) => Ok(journal),
            Self::Unavailable | Self::Committing { .. } => Err(FileSystemError::InvalidOperation),
        }
    }

    pub(in crate::fs::ext4) fn ready_mut(&mut self) -> Result<&mut Journal, FileSystemError> {
        match self {
            Self::Ready(journal) => Ok(journal),
            Self::Unavailable | Self::Committing { .. } => Err(FileSystemError::InvalidOperation),
        }
    }

    pub(in crate::fs::ext4) fn copy_staged(&self, block: u64, output: &mut [u8]) -> bool {
        let bytes = match self {
            Self::Ready(journal) => {
                return journal.copy_staged(block, output);
            }
            Self::Committing { running, active } => active
                .as_ref()
                .and_then(|overlay| overlay.get(block))
                .or_else(|| running.get(block)),
            Self::Unavailable => None,
        };
        let Some(bytes) = bytes else {
            return false;
        };
        output.copy_from_slice(bytes);
        true
    }
}

/// 把 Journal 本体移出 spin owner，在块 I/O 睡眠期间仅留下 immutable read view。
pub(super) struct JournalCommit<'a> {
    fs: &'a Ext4FileSystem,
    journal: Option<Journal>,
    writes: Arc<StagedBlocks>,
    active: Option<Arc<StagedBlocks>>,
}

impl<'a> JournalCommit<'a> {
    pub(super) fn begin(fs: &'a Ext4FileSystem) -> Result<Self, FileSystemError> {
        // Arc control block 必须在状态转换前分配；OOM 时 running transaction 保持完整，
        // 不留下 Committing 空洞。
        let mut writes =
            Arc::<StagedBlocks>::try_new_uninit().map_err(|_| FileSystemError::OutOfMemory)?;
        // caller 持有唯一 FS mutation owner；此快照在锁外 allocation 期间不会改变。
        let has_active = fs.journal.lock().ready_mut_ref()?.active.is_some();
        let mut active_storage = if has_active {
            Some(Arc::<StagedBlocks>::try_new_uninit().map_err(|_| FileSystemError::OutOfMemory)?)
        } else {
            None
        };
        let mut owner = fs.journal.lock();
        let current = core::mem::replace(&mut *owner, JournalOwner::Unavailable);
        let mut journal = match current {
            JournalOwner::Ready(journal) => journal,
            state => {
                *owner = state;
                return Err(FileSystemError::InvalidOperation);
            }
        };
        // running 在 commit 后整体丢弃：已释放 range 随之解除，可再次分配。
        let RunningTransaction { staged, .. } = core::mem::take(&mut journal.running);
        Arc::get_mut(&mut writes)
            .expect("unpublished commit view must be unique")
            .write(staged);
        // SAFETY: unique Arc storage was initialized exactly once above.
        let writes = unsafe { writes.assume_init() };
        let active = journal.active.as_mut().map(|handle| {
            let mut storage = active_storage
                .take()
                .expect("mutation owner changed active journal handle");
            Arc::get_mut(&mut storage)
                .expect("unpublished active view must be unique")
                .write(core::mem::take(&mut handle.staged));
            // SAFETY: unique Arc storage was initialized exactly once; no copy of node/image owners.
            unsafe { storage.assume_init() }
        });
        *owner = JournalOwner::Committing {
            running: writes.clone(),
            active: active.clone(),
        };
        drop(owner);
        Ok(Self {
            fs,
            journal: Some(journal),
            writes,
            active,
        })
    }

    pub(super) fn commit(mut self) -> Result<(), FileSystemError> {
        let journal = self
            .journal
            .as_mut()
            .expect("commit journal restored twice");
        let result = journal.commit_inner(self.fs, &self.writes);
        if result.is_err() {
            journal.failed = true;
            self.fs.metadata_cache.lock().clear();
        }
        self.restore();
        result
    }

    fn restore(&mut self) {
        let mut journal = self.journal.take().expect("commit journal restored twice");
        let mut owner = self.fs.journal.lock();
        let current = core::mem::replace(&mut *owner, JournalOwner::Unavailable);
        let JournalOwner::Committing { running, active } = current else {
            panic!("journal commit view changed owner");
        };
        assert!(
            Arc::ptr_eq(&running, &self.writes),
            "journal running view changed owner"
        );
        assert!(
            match (&active, &self.active) {
                (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
                (None, None) => true,
                _ => false,
            },
            "journal active view changed owner"
        );
        drop(active);
        if let Some(active) = self.active.take() {
            journal
                .active
                .as_mut()
                .expect("commit lost active handle")
                .staged = Arc::try_unwrap(active)
                .unwrap_or_else(|_| panic!("active commit view escaped journal read lock"));
        }
        *owner = JournalOwner::Ready(journal);
    }
}

impl Drop for JournalCommit<'_> {
    fn drop(&mut self) {
        if self.journal.is_some() {
            self.restore();
        }
    }
}
