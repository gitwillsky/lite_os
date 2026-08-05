const FENCE_TIMELINE_CAPACITY: usize = 64;

#[derive(Clone, Copy)]
struct FenceEntry {
    fence: u64,
    complete: bool,
}

const EMPTY_ENTRY: FenceEntry = FenceEntry {
    fence: 0,
    complete: false,
};

/// @description 保存 adapter 已公开 fence 的提交顺序与乱序完成事实。
struct FenceTimeline {
    entries: [FenceEntry; FENCE_TIMELINE_CAPACITY],
    head: usize,
    count: usize,
    completed: u64,
}

impl FenceTimeline {
    /// @description 构造尚无公开 submission 的 fence timeline。
    /// @return completed watermark 与 pending ring 均为空的 owner。
    const fn new() -> Self {
        Self {
            entries: [EMPTY_ENTRY; FENCE_TIMELINE_CAPACITY],
            head: 0,
            count: 0,
            completed: 0,
        }
    }

    /// @description 按 adapter publication 顺序登记一个 userspace 可等待的 fence。
    /// @param fence adapter 返回的非零单调 fence；内部 command fence 可以形成空洞。
    /// @errors fence 非单调或超过 controlq 最大 descriptor head 数时返回 unit error。
    fn submit(&mut self, fence: u64) -> Result<(), ()> {
        let previous = if self.count == 0 {
            self.completed
        } else {
            let tail = (self.head + self.count - 1) % FENCE_TIMELINE_CAPACITY;
            self.entries[tail].fence
        };
        if fence == 0 || fence <= previous || self.count == FENCE_TIMELINE_CAPACITY {
            return Err(());
        }
        let tail = (self.head + self.count) % FENCE_TIMELINE_CAPACITY;
        self.entries[tail] = FenceEntry {
            fence,
            complete: false,
        };
        self.count += 1;
        Ok(())
    }

    /// @description 记录一个 exact fence completion，并推进连续公开 submission 水位。
    /// @param fence device response 已验证的 exact operation fence。
    /// @return 所有更早公开 fence 都完成后可安全暴露给 waiter 的新水位。
    /// @errors fence 未登记或重复完成时返回 unit error。
    fn complete(&mut self, fence: u64) -> Result<u64, ()> {
        let entry = (0..self.count)
            .map(|offset| (self.head + offset) % FENCE_TIMELINE_CAPACITY)
            .find(|&index| self.entries[index].fence == fence)
            .ok_or(())?;
        if self.entries[entry].complete {
            return Err(());
        }
        self.entries[entry].complete = true;
        while self.count != 0 && self.entries[self.head].complete {
            self.completed = self.entries[self.head].fence;
            self.entries[self.head] = EMPTY_ENTRY;
            self.head = (self.head + 1) % FENCE_TIMELINE_CAPACITY;
            self.count -= 1;
        }
        Ok(self.completed)
    }

    /// @description 返回所有更早公开 submission 均已完成的 fence 水位。
    /// @return waiter 可安全使用 `>= fence` 比较的单调值。
    const fn completed(&self) -> u64 {
        self.completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_order_completion_waits_for_earlier_public_fence() {
        let mut timeline = FenceTimeline::new();
        timeline.submit(3).unwrap();
        timeline.submit(5).unwrap();
        timeline.submit(9).unwrap();

        assert_eq!(timeline.complete(9), Ok(0));
        assert_eq!(timeline.completed(), 0);
        assert_eq!(timeline.complete(3), Ok(3));
        assert_eq!(timeline.complete(5), Ok(9));
        assert_eq!(timeline.completed(), 9);
    }

    #[test]
    fn unknown_and_duplicate_completion_are_rejected() {
        let mut timeline = FenceTimeline::new();
        timeline.submit(7).unwrap();

        assert_eq!(timeline.complete(8), Err(()));
        assert_eq!(timeline.complete(7), Ok(7));
        assert_eq!(timeline.complete(7), Err(()));
    }
}
