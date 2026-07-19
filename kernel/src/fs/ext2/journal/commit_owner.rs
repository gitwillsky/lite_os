use alloc::{sync::Arc, vec::Vec};

use crate::fallible_tree::FallibleMap;

use super::{ActiveTransaction, Ext2FileSystem, FileSystemError, Journal};

/// Journal runtime 的唯一状态；commit loan 期间只发布不可变 staged view。
pub(in crate::fs::ext2) enum JournalOwner {
    Unavailable,
    Ready(Journal),
    Committing(Arc<FallibleMap<u32, Vec<u8>>>),
}

impl JournalOwner {
    pub(in crate::fs::ext2) const fn unavailable() -> Self {
        Self::Unavailable
    }

    pub(in crate::fs::ext2) fn install(&mut self, journal: Journal) {
        assert!(matches!(self, Self::Unavailable));
        *self = Self::Ready(journal);
    }

    pub(in crate::fs::ext2) fn ready_mut(&mut self) -> Result<&mut Journal, FileSystemError> {
        match self {
            Self::Ready(journal) => Ok(journal),
            Self::Unavailable | Self::Committing(_) => Err(FileSystemError::InvalidOperation),
        }
    }

    pub(in crate::fs::ext2) fn copy_staged(&self, block: u32, output: &mut [u8]) -> bool {
        let bytes = match self {
            Self::Ready(journal) => {
                return journal.copy_staged(block, output);
            }
            Self::Committing(writes) => writes.get(&block),
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
    fs: &'a Ext2FileSystem,
    journal: Option<Journal>,
    writes: Arc<FallibleMap<u32, Vec<u8>>>,
}

impl<'a> JournalCommit<'a> {
    pub(super) fn begin(fs: &'a Ext2FileSystem) -> Result<Self, FileSystemError> {
        // Arc control block 必须在状态转换前分配；OOM 时 active transaction 仍可由
        // MutationGuard::drop 完整 abort，不留下 Committing 空洞。
        let mut writes = Arc::<FallibleMap<u32, Vec<u8>>>::try_new_uninit()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut owner = fs.journal.lock();
        let current = core::mem::replace(&mut *owner, JournalOwner::Unavailable);
        let mut journal = match current {
            JournalOwner::Ready(journal) => journal,
            state => {
                *owner = state;
                return Err(FileSystemError::InvalidOperation);
            }
        };
        let ActiveTransaction {
            writes: staged,
            allocation_dirty,
        } = match journal.active.take() {
            Some(active) => active,
            None => {
                *owner = JournalOwner::Ready(journal);
                return Err(FileSystemError::InvalidOperation);
            }
        };
        assert!(allocation_dirty.is_empty());
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
