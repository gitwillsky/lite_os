/// 一次 deferred console wake 最多摘取的 waiter 数。
pub(super) const CONSOLE_WAKE_BATCH: usize = 32;

/// @description 单批 console wake 的栈上选择状态；不拥有跨批持久状态。
pub(super) struct ConsoleWakeBatch {
    selected: usize,
    groups: [Option<usize>; CONSOLE_WAKE_BATCH],
}

impl ConsoleWakeBatch {
    pub(super) const fn new() -> Self {
        Self {
            selected: 0,
            groups: [None; CONSOLE_WAKE_BATCH],
        }
    }

    pub(super) const fn is_full(&self) -> bool {
        self.selected == CONSOLE_WAKE_BATCH
    }

    pub(super) const fn selected(&self) -> usize {
        self.selected
    }

    pub(super) fn groups(&self) -> &[Option<usize>] {
        &self.groups[..self.selected]
    }

    pub(super) fn record(&mut self, group: Option<usize>) {
        assert!(!self.is_full(), "console wake batch overflow");
        if let Some(group) = group {
            assert!(
                !self.groups().contains(&Some(group)),
                "console wake group selected twice in one batch"
            );
            self.groups[self.selected] = Some(group);
        }
        self.selected += 1;
    }
}
