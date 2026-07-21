//! evdev 输入：设备发现、包（SYN_REPORT）边界消费、坐标 / 按钮转换累积。
//!
//! - keyboard：设备名含 "keyboard"，EV_KEY 原样转发给焦点窗口（无焦点则丢弃）。
//! - tablet：设备名含 "tablet"，绝对坐标线性映射到屏幕；按钮转换累积到包边界
//!   （`SYN_REPORT`）后统一交由 `pointer` 语义层派发（窗口管理动作、拖动 /
//!   resize 与 `INPUT_POINTER` 转发见 `pointer.rs`）。两个设备都 `EVIOCGRAB`。
//!
//! 所有事件在包边界后统一派发，保证一个包内的坐标与按钮转换按同一光标位置
//! 求值。

use display_proto::InputKey;
use linux_uapi::input::{AbsoluteAxis, InputDevice, InputEvent};
use std::{
    os::fd::{AsFd, BorrowedFd},
    path::PathBuf,
};

use crate::{
    clients::Clients,
    cursor,
    pointer::{Drag, PointerShell},
    window::{Region, Windows},
};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_LEFT: u16 = 272;
const BTN_RIGHT: u16 = 273;
const BTN_MIDDLE: u16 = 274;

const EVENT_CAPACITY: usize = 64;

pub struct Input {
    /// keyboard evdev 设备；未发现时桌面仍可用指针工作。
    keyboard: Option<InputDevice>,
    /// tablet evdev 设备；未发现时桌面仍可用键盘工作。
    tablet: Option<InputDevice>,
    abs_x_range: (i32, i32),
    abs_y_range: (i32, i32),
    /// 光标屏幕坐标（热点）。
    pub cursor_x: i32,
    /// 光标屏幕坐标（热点）。
    pub cursor_y: i32,
    /// 指针按键位掩码（bit0 = left，bit1 = right，bit2 = middle）。
    pub(crate) buttons: u32,
    /// 左键拖动状态（移动 / resize），语义见 `pointer`。
    pub(crate) drag: Option<Drag>,
    /// 左键按下时命中的标题栏按钮（surface id + 区域）；抬起时仍在同一按钮
    /// 内才生效。
    pub(crate) armed: Option<(u32, Region)>,
    pending_x: Option<i32>,
    pending_y: Option<i32>,
    pending_buttons: [(u32, i32); 8],
    pending_button_count: usize,
}

impl Input {
    /// 扫描 `/dev/input/event0..15` 发现并 grab keyboard / tablet，查询 tablet
    /// 绝对轴范围；光标初始位于屏幕中心。
    pub fn open(screen_width: i32, screen_height: i32) -> Self {
        let keyboard = open_matching("keyboard");
        let tablet = open_matching("tablet");
        let abs_x_range = tablet
            .as_ref()
            .and_then(|device| device.absolute_range(AbsoluteAxis::X).ok())
            .map_or((0, 0), |range| (range.minimum, range.maximum));
        let abs_y_range = tablet
            .as_ref()
            .and_then(|device| device.absolute_range(AbsoluteAxis::Y).ok())
            .map_or((0, 0), |range| (range.minimum, range.maximum));
        Self {
            keyboard,
            tablet,
            abs_x_range,
            abs_y_range,
            cursor_x: screen_width / 2,
            cursor_y: screen_height / 2,
            buttons: 0,
            drag: None,
            armed: None,
            pending_x: None,
            pending_y: None,
            pending_buttons: [(0, 0); 8],
            pending_button_count: 0,
        }
    }

    pub fn keyboard_fd(&self) -> Option<BorrowedFd<'_>> {
        self.keyboard.as_ref().map(|device| device.file().as_fd())
    }

    pub fn tablet_fd(&self) -> Option<BorrowedFd<'_>> {
        self.tablet.as_ref().map(|device| device.file().as_fd())
    }

    /// 消费 keyboard 上所有待读事件；EV_KEY 转发给焦点窗口。
    pub fn poll_keyboard(&mut self, windows: &Windows, clients: &Clients) {
        let Some(keyboard) = self.keyboard.as_mut() else {
            return;
        };
        let mut events = [InputEvent::EMPTY; EVENT_CAPACITY];
        let count = read_events(keyboard, &mut events);
        let Some(focused) = windows.focused() else {
            return;
        };
        let Some(window) = windows.get(focused) else {
            return;
        };
        for event in &events[..count] {
            if event.kind() != EV_KEY {
                continue;
            }
            let message = InputKey {
                surface_id: window.surface_id,
                code: u32::from(event.code()),
                value: event.value(),
            };
            let mut buffer = [0u8; 32];
            if let Some(length) = message.encode(&mut buffer) {
                clients.send(window.client, &buffer[..length]);
            }
        }
    }

    /// 消费 tablet 上所有待读事件，按包边界统一交给 `pointer` 语义层派发。
    pub fn poll_tablet(&mut self, shell: &mut PointerShell) {
        let Some(tablet) = self.tablet.as_mut() else {
            return;
        };
        let mut events = [InputEvent::EMPTY; EVENT_CAPACITY];
        let count = read_events(tablet, &mut events);
        for event in &events[..count] {
            match event.kind() {
                EV_ABS => match event.code() {
                    ABS_X => self.pending_x = Some(event.value()),
                    ABS_Y => self.pending_y = Some(event.value()),
                    _ => {}
                },
                EV_KEY => {
                    if let Some(bit) = button_bit(event.code())
                        && self.pending_button_count < self.pending_buttons.len()
                    {
                        self.pending_buttons[self.pending_button_count] = (bit, event.value());
                        self.pending_button_count += 1;
                    }
                }
                EV_SYN if event.code() == SYN_REPORT => {
                    self.dispatch(shell);
                }
                _ => {}
            }
        }
        // 设备异常（无 SYN 的包）时也把积压在包尾的输入落掉，避免永久卡位。
        if self.pending_x.is_some() || self.pending_y.is_some() || self.pending_button_count != 0 {
            self.dispatch(shell);
        }
    }

    /// 一个包内的坐标与按钮转换统一在此生效。
    fn dispatch(&mut self, shell: &mut PointerShell) {
        let previous = cursor::rect_at(self.cursor_x, self.cursor_y);
        let mut moved = false;
        if let Some(raw) = self.pending_x.take() {
            let x = map_absolute(raw, self.abs_x_range, shell.screen_width);
            if x != self.cursor_x {
                self.cursor_x = x;
                moved = true;
            }
        }
        if let Some(raw) = self.pending_y.take() {
            let y = map_absolute(raw, self.abs_y_range, shell.screen_height);
            if y != self.cursor_y {
                self.cursor_y = y;
                moved = true;
            }
        }
        if moved {
            shell.damage.add(previous);
            shell
                .damage
                .add(cursor::rect_at(self.cursor_x, self.cursor_y));
        }
        for index in 0..self.pending_button_count {
            let (bit, value) = self.pending_buttons[index];
            match value {
                1 => self.press(bit, shell),
                0 => self.release(bit, shell),
                _ => {}
            }
        }
        self.pending_button_count = 0;
        if moved {
            self.motion(shell);
        }
    }
}

fn button_bit(code: u16) -> Option<u32> {
    match code {
        BTN_LEFT => Some(1),
        BTN_RIGHT => Some(2),
        BTN_MIDDLE => Some(4),
        _ => None,
    }
}

/// 把 tablet 绝对坐标从 `[min, max]` 线性映射到 `[0, extent)`。
fn map_absolute(raw: i32, range: (i32, i32), extent: i32) -> i32 {
    let (minimum, maximum) = range;
    if maximum <= minimum || extent <= 0 {
        return 0;
    }
    let scaled = i64::from(raw - minimum) * i64::from(extent - 1) / i64::from(maximum - minimum);
    (scaled as i32).clamp(0, extent - 1)
}

/// 读取所有待读事件到 `events`（不越过包边界语义，包边界由调用方识别）。
fn read_events(device: &mut InputDevice, events: &mut [InputEvent]) -> usize {
    let mut total = 0;
    while total < events.len() {
        match device.read_events(&mut events[total..]) {
            Ok(0) => break,
            Ok(count) => total += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    total
}

/// 扫描 `/dev/input/event0..15`，返回首个名称含 `needle` 且已 grab 的 fd。
fn open_matching(needle: &str) -> Option<InputDevice> {
    for index in 0..16u32 {
        let path = PathBuf::from(format!("/dev/input/event{index}"));
        let Ok(device) = InputDevice::open(&path) else {
            continue;
        };
        if device
            .name()
            .is_ok_and(|name| name.to_ascii_lowercase().contains(needle))
            && device.grab().is_ok()
        {
            return Some(device);
        }
    }
    None
}
