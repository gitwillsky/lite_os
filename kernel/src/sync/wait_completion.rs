use core::sync::atomic::{AtomicU8, Ordering};

const REQUESTED: u8 = 0;
const ARMING: u8 = 1;
const SLEEPING: u8 = 2;
const COMPLETED: u8 = 3;

/// @description owner completion 与 scheduler membership publication 的无丢唤醒握手。
///
/// OWNER: request/wait node 在发布前 `reset`，并独占 token 直到 completion 被消费；
/// 缺失单一 token 会让 completion 与 scheduler membership 分裂并丢失 wake。
pub(crate) struct WaitCompletion(AtomicU8);

impl WaitCompletion {
    /// @description 构造尚无 active wait 的 completed token。
    pub(crate) const fn new() -> Self {
        Self(AtomicU8::new(COMPLETED))
    }

    /// @description 在外部 owner publication 前开始一次 wait。
    pub(crate) fn reset(&self) {
        assert_eq!(
            self.0.swap(REQUESTED, Ordering::AcqRel),
            COMPLETED,
            "active wait completion reset twice"
        );
    }

    /// @description 查询 owner 是否已完成本次 wait。
    pub(crate) fn is_complete(&self) -> bool {
        self.0.load(Ordering::Acquire) == COMPLETED
    }

    /// @description 在 scheduler membership publication 前取得 arming ownership。
    pub(crate) fn begin_arming(&self) -> bool {
        self.0
            .compare_exchange(REQUESTED, ARMING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// @description membership 已存在后发布 sleeping。
    /// @return completion 在 publication 竞态中抢先发生时为 `true`。
    pub(crate) fn finish_arming(&self) -> bool {
        self.0
            .compare_exchange(ARMING, SLEEPING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    }

    /// @description 发布 owner completion。
    /// @return target 已进入 sleeping、需要 exact wake 时为 `true`。
    pub(crate) fn complete(&self) -> bool {
        self.0.swap(COMPLETED, Ordering::AcqRel) == SLEEPING
    }
}

#[cfg(test)]
mod tests {
    use super::WaitCompletion;

    #[test]
    fn completion_covers_all_publication_orders() {
        let before = WaitCompletion::new();
        before.reset();
        assert!(!before.complete());
        assert!(!before.begin_arming());

        let during = WaitCompletion::new();
        during.reset();
        assert!(during.begin_arming());
        assert!(!during.complete());
        assert!(during.finish_arming());

        let after = WaitCompletion::new();
        after.reset();
        assert!(after.begin_arming());
        assert!(!after.finish_arming());
        assert!(after.complete());
        assert!(after.is_complete());
    }
}
