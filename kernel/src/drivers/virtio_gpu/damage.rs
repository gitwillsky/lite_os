use crate::drivers::DisplayRect;

pub(super) const MAX_DAMAGE_RECTS: usize = 32;

/// @description controlq 内无分配保存并推进一次 DIRTYFB clip transaction。
pub(super) struct DamageTransition {
    rectangles: [DisplayRect; MAX_DAMAGE_RECTS],
    count: u8,
    index: u8,
}

impl DamageTransition {
    /// @description 构造尚未承载 active damage operation 的固定 scratch。
    /// @return 所有 clip 为零且 cursor 为空的 state。
    pub(super) const fn new() -> Self {
        Self {
            rectangles: [DisplayRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }; MAX_DAMAGE_RECTS],
            count: 0,
            index: 0,
        }
    }

    /// @description 以已验证的 fixed clip copy 开始一次 damage operation。
    /// @param rectangles 不再访问 userspace 的完整固定副本。
    /// @param count 有效 prefix，必须位于 1..=MAX_DAMAGE_RECTS。
    /// @return 无返回值；非法 count 表示 caller contract 损坏并 fail-stop。
    pub(super) fn begin(&mut self, rectangles: [DisplayRect; MAX_DAMAGE_RECTS], count: usize) {
        assert!(
            (1..=MAX_DAMAGE_RECTS).contains(&count),
            "invalid VirtIO GPU damage clip count"
        );
        self.rectangles = rectangles;
        self.count = count as u8;
        self.index = 0;
    }

    /// @description 返回尚未完成 transfer/flush 的当前 clip。
    /// @return begin 后当前 cursor 对应的 rectangle。
    pub(super) fn current(&self) -> DisplayRect {
        self.rectangles[usize::from(self.index)]
    }

    /// @description 在当前 clip flush completion 后推进 cursor。
    /// @return 下一 clip；全部完成时返回 None。
    pub(super) fn advance(&mut self) -> Option<DisplayRect> {
        self.index += 1;
        (self.index < self.count).then(|| self.current())
    }
}
