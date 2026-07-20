//! evdev 输入：设备发现、包（SYN_REPORT）边界消费、指针 / 键盘语义派发。
//!
//! - keyboard：设备名含 "keyboard"，EV_KEY 原样转发给焦点窗口（无焦点则丢弃）。
//! - tablet：设备名含 "tablet"，绝对坐标线性映射到屏幕；按钮维护 bitmask，
//!   命中关闭按钮（按下 + 抬起均在按钮内）发 `CLOSE_REQUEST`，标题栏按下进入
//!   拖动，内容区按下 raise + focus 并转发 `INPUT_POINTER`；无键悬停移动同样
//!   转发。两个设备都 `EVIOCGRAB`。
//!
//! 所有事件在包边界（`SYN_REPORT`）后统一派发，保证一个包内的坐标与按钮
//! 转换按同一光标位置求值。

use display_proto::{CloseRequest, Focus, InputKey, InputPointer};

use crate::{
    compositor::Damage,
    cursor, ffi,
    ffi::{InputAbsInfo, InputEvent},
    scanout::Rect,
    server::Clients,
    window::{Region, Window, Windows},
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

const BUTTON_LEFT: u32 = 1;
/// 拖动 / 关闭判定只对左键生效。
const EVENT_CAPACITY: usize = 64;

const EMPTY_EVENT: InputEvent = InputEvent {
    seconds: 0,
    microseconds: 0,
    kind: 0,
    code: 0,
    value: 0,
};

/// 左键按下标题栏后进入的拖动状态。
#[derive(Clone, Copy)]
struct Drag {
    surface_id: u32,
    /// 按下点相对窗口外框原点的偏移。
    offset_x: i32,
    offset_y: i32,
}

pub struct Input {
    /// keyboard evdev fd；未发现设备时为 -1（桌面仍可用指针工作）。
    pub keyboard_fd: i32,
    /// tablet evdev fd；未发现时为 -1。
    pub tablet_fd: i32,
    abs_x_range: (i32, i32),
    abs_y_range: (i32, i32),
    /// 光标屏幕坐标（热点）。
    pub cursor_x: i32,
    /// 光标屏幕坐标（热点）。
    pub cursor_y: i32,
    buttons: u32,
    drag: Option<Drag>,
    /// 左键按下时命中的关闭按钮所属 surface；抬起时仍在同一按钮内才发
    /// `CLOSE_REQUEST`。
    close_armed: Option<u32>,
    pending_x: Option<i32>,
    pending_y: Option<i32>,
    pending_buttons: [(u32, i32); 8],
    pending_button_count: usize,
}

impl Input {
    /// 扫描 `/dev/input/event0..15` 发现并 grab keyboard / tablet，查询 tablet
    /// 绝对轴范围；光标初始位于屏幕中心。
    pub fn open(screen_width: i32, screen_height: i32) -> Self {
        let keyboard_fd = open_matching(b"keyboard");
        let tablet_fd = open_matching(b"tablet");
        let mut input = Self {
            keyboard_fd,
            tablet_fd,
            abs_x_range: (0, 0),
            abs_y_range: (0, 0),
            cursor_x: screen_width / 2,
            cursor_y: screen_height / 2,
            buttons: 0,
            drag: None,
            close_armed: None,
            pending_x: None,
            pending_y: None,
            pending_buttons: [(0, 0); 8],
            pending_button_count: 0,
        };
        if tablet_fd >= 0 {
            input.abs_x_range = abs_range(tablet_fd, ffi::EVIOCGABS_X);
            input.abs_y_range = abs_range(tablet_fd, ffi::EVIOCGABS_Y);
        }
        input
    }

    /// 消费 keyboard 上所有待读事件；EV_KEY 转发给焦点窗口。
    pub fn poll_keyboard(&mut self, windows: &Windows, clients: &Clients) {
        if self.keyboard_fd < 0 {
            return;
        }
        let mut events = [EMPTY_EVENT; EVENT_CAPACITY];
        let count = read_events(self.keyboard_fd, &mut events);
        let Some(focused) = windows.focused() else {
            return;
        };
        let Some(window) = windows.get(focused) else {
            return;
        };
        for event in &events[..count] {
            if event.kind != EV_KEY {
                continue;
            }
            let message = InputKey {
                surface_id: window.surface_id,
                code: u32::from(event.code),
                value: event.value,
            };
            let mut buffer = [0u8; 32];
            if let Some(length) = message.encode(&mut buffer) {
                clients.send(window.client, &buffer[..length]);
            }
        }
    }

    /// 消费 tablet 上所有待读事件，按包边界统一派发。
    pub fn poll_tablet(
        &mut self,
        windows: &mut Windows,
        clients: &Clients,
        damage: &mut Damage,
        screen_width: i32,
        screen_height: i32,
    ) {
        if self.tablet_fd < 0 {
            return;
        }
        let mut events = [EMPTY_EVENT; EVENT_CAPACITY];
        let count = read_events(self.tablet_fd, &mut events);
        for event in &events[..count] {
            match event.kind {
                EV_ABS => match event.code {
                    ABS_X => self.pending_x = Some(event.value),
                    ABS_Y => self.pending_y = Some(event.value),
                    _ => {}
                },
                EV_KEY => {
                    if let Some(bit) = button_bit(event.code)
                        && self.pending_button_count < self.pending_buttons.len()
                    {
                        self.pending_buttons[self.pending_button_count] = (bit, event.value);
                        self.pending_button_count += 1;
                    }
                }
                EV_SYN if event.code == SYN_REPORT => {
                    self.dispatch(windows, clients, damage, screen_width, screen_height);
                }
                _ => {}
            }
        }
        // 设备异常（无 SYN 的包）时也把积压在包尾的输入落掉，避免永久卡位。
        if self.pending_x.is_some()
            || self.pending_y.is_some()
            || self.pending_button_count != 0
        {
            self.dispatch(windows, clients, damage, screen_width, screen_height);
        }
    }

    /// 一个包内的坐标与按钮转换统一在此生效。
    fn dispatch(
        &mut self,
        windows: &mut Windows,
        clients: &Clients,
        damage: &mut Damage,
        screen_width: i32,
        screen_height: i32,
    ) {
        let previous = cursor::rect_at(self.cursor_x, self.cursor_y);
        let mut moved = false;
        if let Some(raw) = self.pending_x.take() {
            let x = map_absolute(raw, self.abs_x_range, screen_width);
            if x != self.cursor_x {
                self.cursor_x = x;
                moved = true;
            }
        }
        if let Some(raw) = self.pending_y.take() {
            let y = map_absolute(raw, self.abs_y_range, screen_height);
            if y != self.cursor_y {
                self.cursor_y = y;
                moved = true;
            }
        }
        if moved {
            damage.add(previous);
            damage.add(cursor::rect_at(self.cursor_x, self.cursor_y));
        }
        for index in 0..self.pending_button_count {
            let (bit, value) = self.pending_buttons[index];
            match value {
                1 => self.press(bit, windows, clients, damage),
                0 => self.release(bit, windows, clients),
                _ => {}
            }
        }
        self.pending_button_count = 0;
        if moved {
            self.motion(windows, clients, damage, screen_width, screen_height);
        }
    }

    fn press(
        &mut self,
        bit: u32,
        windows: &mut Windows,
        clients: &Clients,
        damage: &mut Damage,
    ) {
        self.buttons |= bit;
        match windows.hit_test(self.cursor_x, self.cursor_y) {
            Some((slot, Region::CloseButton)) if bit == BUTTON_LEFT => {
                if let Some(window) = windows.get(slot) {
                    self.close_armed = Some(window.surface_id);
                }
            }
            Some((slot, Region::TitleBar)) if bit == BUTTON_LEFT => {
                focus_raise(windows, clients, damage, slot);
                if let Some(window) = windows.get(slot) {
                    self.drag = Some(Drag {
                        surface_id: window.surface_id,
                        offset_x: self.cursor_x - window.x,
                        offset_y: self.cursor_y - window.y,
                    });
                }
            }
            Some((slot, Region::Content)) => {
                focus_raise(windows, clients, damage, slot);
                self.forward_pointer(windows, clients, slot);
            }
            _ => {}
        }
    }

    fn release(&mut self, bit: u32, windows: &mut Windows, clients: &Clients) {
        self.buttons &= !bit;
        if bit == BUTTON_LEFT {
            if let Some(armed) = self.close_armed.take() {
                let confirmed = windows
                    .by_surface(armed)
                    .is_some_and(|slot| {
                        matches!(
                            windows.hit_test(self.cursor_x, self.cursor_y),
                            Some((hit, Region::CloseButton)) if hit == slot
                        )
                    });
                if confirmed
                    && let Some(slot) = windows.by_surface(armed)
                    && let Some(window) = windows.get(slot)
                {
                    let message = CloseRequest {
                        surface_id: window.surface_id,
                    };
                    let mut buffer = [0u8; 16];
                    if let Some(length) = message.encode(&mut buffer) {
                        clients.send(window.client, &buffer[..length]);
                    }
                }
            }
            self.drag = None;
        }
        // 让焦点窗口看到按键释放（指针在其内容区内时）。
        if let Some(focused) = windows.focused() {
            self.forward_pointer(windows, clients, focused);
        }
    }

    /// 无按钮转换的光标移动：拖动窗口、悬停转发或拖动中转发。
    fn motion(
        &mut self,
        windows: &mut Windows,
        clients: &Clients,
        damage: &mut Damage,
        screen_width: i32,
        screen_height: i32,
    ) {
        if let Some(drag) = self.drag {
            let Some(slot) = windows.by_surface(drag.surface_id) else {
                self.drag = None;
                return;
            };
            let Some(window) = windows.get_mut(slot) else {
                self.drag = None;
                return;
            };
            let old = window.outer_rect();
            //  clamp：至少保留 32px 可点区域在屏内，标题栏不推出上沿。
            let layout = window.layout();
            let new_x = (self.cursor_x - drag.offset_x)
                .clamp(32 - layout.outer_width, screen_width - 32);
            let new_y = (self.cursor_y - drag.offset_y).clamp(0, screen_height - 32);
            if new_x != window.x || new_y != window.y {
                window.x = new_x;
                window.y = new_y;
                damage.add(old);
                damage.add(window.outer_rect());
            }
        } else if self.buttons == 0 {
            if let Some((slot, Region::Content)) = windows.hit_test(self.cursor_x, self.cursor_y)
            {
                self.forward_pointer(windows, clients, slot);
            }
        } else if let Some(focused) = windows.focused() {
            self.forward_pointer(windows, clients, focused);
        }
    }

    /// 指针在窗口内容区内时转发 `INPUT_POINTER`（内容相对坐标 + buttons）。
    fn forward_pointer(&self, windows: &Windows, clients: &Clients, slot: usize) {
        let Some(window) = windows.get(slot) else {
            return;
        };
        let content = window.content_rect();
        let x = self.cursor_x - content.x1;
        let y = self.cursor_y - content.y1;
        if !(0..content.width()).contains(&x) || !(0..content.height()).contains(&y) {
            return;
        }
        let message = InputPointer {
            surface_id: window.surface_id,
            x: x as u32,
            y: y as u32,
            buttons: self.buttons,
            wheel: 0,
        };
        let mut buffer = [0u8; 32];
        if let Some(length) = message.encode(&mut buffer) {
            clients.send(window.client, &buffer[..length]);
        }
    }
}

/// raise 窗口并把键盘焦点切过去（附带 `FOCUS` 消息与标题栏重画）。
pub fn focus_raise(
    windows: &mut Windows,
    clients: &Clients,
    damage: &mut Damage,
    slot: usize,
) {
    windows.raise(slot);
    set_focus(windows, clients, damage, Some(slot));
}

/// 切换键盘焦点：旧焦点发 `FOCUS{0}`、新焦点发 `FOCUS{1}`，两侧标题栏
/// 记入 damage（焦点色变化）。
pub fn set_focus(
    windows: &mut Windows,
    clients: &Clients,
    damage: &mut Damage,
    slot: Option<usize>,
) {
    if windows.focused() == slot {
        return;
    }
    if let Some(old) = windows.focused()
        && let Some(window) = windows.get(old)
    {
        send_focus(clients, window, 0);
        damage.add(title_bar_strip(window));
    }
    windows.set_focus(slot);
    if let Some(new) = slot
        && let Some(window) = windows.get(new)
    {
        send_focus(clients, window, 1);
        damage.add(title_bar_strip(window));
    }
}

fn send_focus(clients: &Clients, window: &Window, focused: u32) {
    let message = Focus {
        surface_id: window.surface_id,
        focused,
    };
    let mut buffer = [0u8; 16];
    if let Some(length) = message.encode(&mut buffer) {
        clients.send(window.client, &buffer[..length]);
    }
}

fn title_bar_strip(window: &Window) -> Rect {
    let outer = window.outer_rect();
    Rect::new(outer.x1, outer.y1, outer.x2, outer.y1 + crate::chrome::TITLE_HEIGHT)
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

fn abs_range(fd: i32, request: usize) -> (i32, i32) {
    let mut info = InputAbsInfo::default();
    // SAFETY: info 在 ioctl 期间有效，内核回写整个 input_absinfo。
    if unsafe { ffi::ioctl(fd, request, (&mut info as *mut InputAbsInfo).cast()) } < 0 {
        return (0, 0);
    }
    (info.minimum, info.maximum)
}

/// 读取所有待读事件到 `events`（不越过包边界语义，包边界由调用方识别）。
fn read_events(fd: i32, events: &mut [InputEvent]) -> usize {
    let mut total = 0;
    while total < events.len() {
        let capacity = (events.len() - total) * size_of::<InputEvent>();
        // SAFETY: events[total..] 在调用期间有效且可写，容量按整块事件对齐。
        let count = unsafe { ffi::read(fd, events[total..].as_mut_ptr().cast(), capacity) };
        if count > 0 {
            total += count as usize / size_of::<InputEvent>();
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else {
            break;
        }
    }
    total
}

/// 扫描 `/dev/input/event0..15`，返回首个名称含 `needle` 且已 grab 的 fd。
fn open_matching(needle: &[u8]) -> i32 {
    for index in 0..16u32 {
        let mut path = [0u8; 32];
        let prefix = b"/dev/input/event";
        path[..prefix.len()].copy_from_slice(prefix);
        let capacity = path.len() - 1;
        let length = prefix.len() + decimal(index, &mut path[prefix.len()..capacity]);
        path[length] = 0;
        let fd = unsafe {
            ffi::open(
                path.as_ptr().cast(),
                ffi::O_RDONLY | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
            )
        };
        if fd < 0 {
            continue;
        }
        let mut name = [0u8; 128];
        let named = unsafe { ffi::ioctl(fd, ffi::EVIOCGNAME_128, name.as_mut_ptr().cast()) } >= 0;
        if named && contains(&name, needle) {
            let mut grab = 1i32;
            // SAFETY: grab 在 ioctl 期间有效。
            unsafe { ffi::ioctl(fd, ffi::EVIOCGRAB, (&mut grab as *mut i32).cast()) };
            return fd;
        }
        // SAFETY: fd 为本函数打开但未选中的描述符。
        unsafe { ffi::close(fd) };
    }
    -1
}

fn contains(name: &[u8], needle: &[u8]) -> bool {
    let mut matched = 0;
    for byte in name.iter().copied().take_while(|byte| *byte != 0) {
        let value = byte.to_ascii_lowercase();
        matched = if value == needle[matched] {
            matched + 1
        } else {
            usize::from(value == needle[0])
        };
        if matched == needle.len() {
            return true;
        }
    }
    false
}

fn decimal(mut value: u32, output: &mut [u8]) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for (index, slot) in output.iter_mut().enumerate().take(count) {
        *slot = digits[count - 1 - index];
    }
    count
}
