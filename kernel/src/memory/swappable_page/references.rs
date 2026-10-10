//! canonical PageState 内的映射代次与引用计数；没有第二个 lock、slot 或 frame owner。
//! 调用者必须在 PageState 的同一短锁中修改本值并提交 physical backing。

/// 一个实际映射持有的逻辑 residency；物理临时 Arc pin 不产生本引用。
#[derive(Debug)]
pub(super) struct MappingReference {
    installed_epoch: u64,
    private: bool,
}

/// 一次迁移的引用集合证据；MM callback 必须单轮、不重复计数且完成全部 translation fence。
#[derive(Debug, Clone, Copy)]
pub(super) struct MigrationSnapshot {
    version: u64,
    total: usize,
}

impl MigrationSnapshot {
    pub(super) fn total(self) -> usize {
        self.total
    }
}

/// 由 canonical PageState 同锁拥有的计数；MappingReference 生命周期负责增删。
#[derive(Debug, Default)]
pub(super) struct MappingState {
    epoch: u64,
    version: u64,
    total: usize,
    private: usize,
    stale: usize,
}

impl MappingState {
    fn changed(&mut self) {
        self.version = self
            .version
            .checked_add(1)
            .expect("page reference version exhausted");
        assert!(self.private <= self.total && self.stale <= self.total);
    }

    /// 为已完成实际 leaf 或 PROT_NONE logical publication 的新映射建立引用。
    /// `epoch` 必须来自 PageOp 下取得的 current-frame snapshot，不能取等待前的值。
    pub(super) fn installed(&mut self, private: bool, epoch: u64) -> MappingReference {
        assert_eq!(
            epoch, self.epoch,
            "mapping published an obsolete physical page"
        );
        self.total = self
            .total
            .checked_add(1)
            .expect("page mapping count exhausted");
        self.private += usize::from(private);
        self.changed();
        MappingReference {
            installed_epoch: epoch,
            private,
        }
    }

    /// fork 继承逻辑引用，不能把一个尚未 fault-in 的 SwapEntry 虚报为驻留。
    pub(super) fn inherit(&mut self, parent: &MappingReference) -> MappingReference {
        assert!(
            parent.installed_epoch <= self.epoch,
            "mapping epoch from the future"
        );
        self.total = self
            .total
            .checked_add(1)
            .expect("page mapping count exhausted");
        self.private += usize::from(parent.private);
        self.stale += usize::from(parent.installed_epoch != self.epoch);
        self.changed();
        MappingReference {
            installed_epoch: parent.installed_epoch,
            private: parent.private,
        }
    }

    /// 仅在对应旧 hardware alias 已撤销且全 CPU fence 完成后释放引用。
    pub(super) fn release(&mut self, reference: MappingReference) {
        self.total = self
            .total
            .checked_sub(1)
            .expect("page mapping released twice");
        self.private = self
            .private
            .checked_sub(usize::from(reference.private))
            .expect("private mapping count underflow");
        self.stale = self
            .stale
            .checked_sub(usize::from(reference.installed_epoch != self.epoch))
            .expect("swap-entry count underflow");
        self.changed();
    }

    /// 成功 fault/swapoff leaf 或 PROT_NONE logical installation 后才更新该映射的 epoch。
    pub(super) fn mark_installed(&mut self, reference: &mut MappingReference, epoch: u64) {
        assert_eq!(
            epoch, self.epoch,
            "mapping installed an obsolete physical page"
        );
        if reference.installed_epoch == epoch {
            return;
        }
        self.stale = self
            .stale
            .checked_sub(1)
            .expect("swap-entry count underflow");
        reference.installed_epoch = epoch;
        self.changed();
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(super) fn stale(&self) -> usize {
        self.stale
    }
    pub(super) fn is_installed(&self, reference: &MappingReference) -> bool {
        reference.installed_epoch == self.epoch
    }
    /// COW 独占只按实际映射计数；调用者还必须验证 Page 的 private origin。
    pub(super) fn private_exclusive(&self, reference: &MappingReference) -> bool {
        reference.private && self.private == 1 && self.total == 1
    }
    pub(super) fn snapshot(&self) -> MigrationSnapshot {
        MigrationSnapshot {
            version: self.version,
            total: self.total,
        }
    }
    /// Weak-dead MM 的待 fence 引用仍计入 total；不匹配时不能释放 frame。
    pub(super) fn covered(&self, snapshot: MigrationSnapshot, fenced_references: usize) -> bool {
        self.version == snapshot.version
            && self.total == snapshot.total
            && fenced_references == snapshot.total
    }
    /// 完整 IO 成功后，在 physical backing publication 的 SAME PageState lock 内调用。
    /// false 保持 epoch 不变，撤销过的叶仍是 logical Resident，可按原 frame 重建。
    pub(super) fn commit_swap(
        &mut self,
        snapshot: MigrationSnapshot,
        fenced_references: usize,
    ) -> bool {
        if !self.covered(snapshot, fenced_references) {
            return false;
        }
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("page swap epoch exhausted");
        self.stale = self.total;
        self.changed();
        true
    }
}
