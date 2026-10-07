use alloc::sync::Arc;

use super::{Ext4FileSystem, FileSystemError, Journal, RunningTransaction, StagedBlocks};

/// Journal runtime 的唯一状态；commit loan 期间只发布不可变 staged view。
pub(in crate::fs::ext4) enum JournalOwner {
    Unavailable,
    Ready(Journal),
    Committing(Arc<StagedBlocks>),
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
            Self::Unavailable | Self::Committing(_) => Err(FileSystemError::InvalidOperation),
        }
    }

    pub(in crate::fs::ext4) fn ready_mut(&mut self) -> Result<&mut Journal, FileSystemError> {
        match self {
            Self::Ready(journal) => Ok(journal),
            Self::Unavailable | Self::Committing(_) => Err(FileSystemError::InvalidOperation),
        }
    }

    pub(in crate::fs::ext4) fn copy_staged(&self, block: u64, output: &mut [u8]) -> bool {
        let bytes = match self {
            Self::Ready(journal) => {
                return journal.copy_staged(block, output);
            }
            Self::Committing(staged) => staged.get(block),
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
}

impl<'a> JournalCommit<'a> {
    pub(super) fn begin(fs: &'a Ext4FileSystem) -> Result<Self, FileSystemError> {
        // Arc control block 必须在状态转换前分配；OOM 时 running transaction 保持完整，
        // 不留下 Committing 空洞。
        let mut writes =
            Arc::<StagedBlocks>::try_new_uninit().map_err(|_| FileSystemError::OutOfMemory)?;
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
        *owner = JournalOwner::Committing(writes.clone());
        drop(owner);
        Ok(Self {
            fs,
            journal: Some(journal),
            writes,
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
        let journal = self.journal.take().expect("commit journal restored twice");
        let mut owner = self.fs.journal.lock();
        assert!(
            matches!(&*owner, JournalOwner::Committing(current) if Arc::ptr_eq(current, &self.writes)),
            "journal commit view changed owner"
        );
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
