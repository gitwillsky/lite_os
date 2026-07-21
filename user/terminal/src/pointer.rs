//! 指针翻译：桌面转发的 `INPUT_POINTER{x, y, buttons, wheel}`（surface 内容相对
//! 像素坐标）编码为 X10 鼠标转义序列写入 PTY。X10 编码逻辑搬自 console-session
//! 的 `reactor/pointer.rs`；原代码把绝对屏坐标按轴量程折算成 cell，现在坐标系
//! 变为内容相对像素，直接除以 cell 尺寸即可，算法其余部分不变。

use display_proto as proto;

use crate::{
    atlas::FontMetrics,
    input::InputQueue,
    model::{Grid, Model},
};

/// 单条 `INPUT_POINTER` 最坏产出的转义字节数：3 个按键边沿 + 4 步滚轮，各 6 字节。
/// 事件循环以此证明本批 report 都能原子进入固定 ring。
pub const MAX_POINTER_BYTES: usize = 7 * 6;

/// 单条消息滚轮步数上限：桌面通常逐步发送 ±1，钳制避免异常大增量冲垮输入 ring。
const MAX_WHEEL_STEPS: i32 = 4;

/// 指针按键状态：`INPUT_POINTER` 携带的是绝对按键掩码，边沿由本结构对比上一帧得出。
pub struct Pointer {
    buttons: u32,
}

impl Pointer {
    pub fn new() -> Self {
        Self { buttons: 0 }
    }

    /// `INPUT_SYNC_RESET` 后清空按键 snapshot，避免掩码边沿判断出错。
    pub fn reset(&mut self) {
        self.buttons = 0;
    }

    /// 处理一条 `INPUT_POINTER`。调用方须保证 `input.remaining() >= MAX_POINTER_BYTES`。
    pub fn handle(
        &mut self,
        input: &mut InputQueue,
        model: &Model,
        metrics: FontMetrics,
        event: &proto::InputPointer,
    ) {
        let column = (event.x as usize / metrics.width())
            .min(model.columns().saturating_sub(1))
            .min(222);
        let row = (event.y as usize / metrics.height())
            .min(model.rows().saturating_sub(1))
            .min(222);
        let previous = self.buttons;
        self.buttons = event.buttons;
        // 协议掩码 bit0 = left / bit1 = right / bit2 = middle；
        // X10 按键码沿用原 evdev 映射：left = 0，middle = 1，right = 2。
        for (bit, button) in [(0, 0u8), (1, 2), (2, 1)] {
            let was = previous & (1 << bit) != 0;
            let now = event.buttons & (1 << bit) != 0;
            if was != now {
                report(input, model, button, now, column, row);
            }
        }
        let wheel = event.wheel.clamp(-MAX_WHEEL_STEPS, MAX_WHEEL_STEPS);
        for _ in 0..wheel.unsigned_abs() {
            let button = if wheel > 0 { 64 } else { 65 };
            report(input, model, button, true, column, row);
        }
    }
}

fn report(
    input: &mut InputQueue,
    model: &Model,
    button: u8,
    pressed: bool,
    column: usize,
    row: usize,
) {
    let mode = model.mouse_mode();
    if mode == 0 || mode == 1 && !pressed {
        return;
    }
    let button = if pressed { button } else { 3 };
    input.push(&[
        0x1b,
        b'[',
        b'M',
        32 + button,
        32 + column as u8 + 1,
        32 + row as u8 + 1,
    ]);
}
