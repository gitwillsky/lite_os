//! 音量状态与布局：音量控制窗口与托盘面板共用；只展示 ALSA 快照；每次操作后与外部更新均重新读取，不复制可写 mixer owner。
extern crate liteos_runtime as _;
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{Ink, Painter, Trackbar, TrackbarDrag};
use linux_uapi::alsa::control::{MasterControl, MasterVolume};
use std::time::{Duration, Instant};

/// 正在拖动的滑块。
enum Drag {
    Volume(TrackbarDrag),
    Balance(TrackbarDrag),
}

pub struct Volume {
    control: Option<MasterControl>,
    pub value: Option<MasterVolume>,
    pub message: &'static str,
    /// 左右声道的偏向：-100 全偏左，+100 全偏右。两个声道相等时为 0；静音为零音量时保留上一次的值，否则拖到 0 再拖起时平衡会丢。
    pub balance: i32,
    // 仅 slider 按下至 release/leave/focus-loss 期间有效，防止普通悬停修改 Master。
    drag: Option<Drag>,
    // 外部状态最多每 100ms 采样；缺少 deadline，指针事件会把 ioctl 查询放大到输入频率。
    next_refresh: Instant,
}
impl Volume {
    pub fn open() -> Self {
        match MasterControl::open() {
            Ok(control) => {
                let mut state = Self {
                    control: Some(control),
                    value: None,
                    message: "",
                    balance: 0,
                    drag: None,
                    next_refresh: Instant::now(),
                };
                state.refresh();
                state
            }
            Err(error) => Self {
                control: None,
                value: None,
                balance: 0,
                drag: None,
                next_refresh: Instant::now(),
                message: if error.kind() == std::io::ErrorKind::PermissionDenied {
                    "此账户无音频设备访问权限。"
                } else {
                    "没有可用的音频输出。"
                },
            },
        }
    }
    pub fn poll(&mut self) -> bool {
        if Instant::now() < self.next_refresh {
            return false;
        }
        self.refresh()
    }
    fn refresh(&mut self) -> bool {
        self.next_refresh = Instant::now() + Duration::from_millis(100);
        let before = self.value;
        if let Some(control) = &self.control {
            match control.get() {
                Ok(value) => {
                    self.value = Some(value);
                    self.message = "";
                    let (quiet, loud) = (value.left.min(value.right), value.left.max(value.right));
                    if loud > 0 {
                        let offset = 100 - i32::from(quiet) * 100 / i32::from(loud);
                        self.balance = if value.right >= value.left {
                            offset
                        } else {
                            -offset
                        };
                    }
                }
                Err(error) => {
                    eprintln!("desktop: ALSA Master read: {error}");
                    self.value = None;
                    self.control = None;
                    self.message = "无法读取音频设备。";
                }
            }
        }
        before != self.value
    }
    /// 按当前平衡把总音量 `percent` 分配到左右声道并设置静音。
    pub fn change(&mut self, percent: u8, muted: bool) {
        let share =
            |toward_other: i32| (i32::from(percent) * (100 - toward_other.max(0)) / 100) as u8;
        let (left, right) = (share(self.balance), share(-self.balance));
        if self
            .value
            .is_some_and(|value| value.left == left && value.right == right && value.muted == muted)
        {
            return;
        }
        if let Some(control) = &self.control
            && let Err(error) = control.set_stereo(left, right, muted)
        {
            eprintln!("desktop: ALSA Master write: {error}");
        }
        self.refresh();
    }

    /// 设置平衡（-100..=100）；总音量不变。
    pub fn set_balance(&mut self, balance: i32) {
        if let Some(value) = self.value {
            self.balance = balance.clamp(-100, 100);
            self.change(value.left.max(value.right), value.muted);
        }
    }
    pub fn key(&mut self, code: u32) {
        if let Some(value) = self.value {
            let current = value.left.max(value.right);
            match code {
                103 => self.change(current.saturating_add(5).min(100), value.muted),
                108 => self.change(current.saturating_sub(5), value.muted),
                57 => self.change(current, !value.muted),
                _ => {}
            }
        }
    }
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }
    pub fn cancel(&mut self) {
        self.drag = None;
    }
    pub fn motion(&mut self, layout: &Layout, x: f64, y: f64) {
        match (&self.drag, self.value, layout.balance_bar) {
            (Some(Drag::Volume(grab)), Some(value), _) => {
                let position = grab.position(Trackbar::new(layout.slider_hit, true), 100, (x, y));
                self.change((100 - position) as u8, value.muted)
            }
            (Some(Drag::Balance(grab)), Some(_), Some(bar)) => {
                let balance = grab.position(Trackbar::new(bar, false), 200, (x, y)) - 100;
                self.set_balance(if balance.abs() <= 6 { 0 } else { balance });
            }
            _ => {}
        }
    }
    pub fn button(&mut self, layout: &Layout, x: f64, y: f64, pressed: bool) {
        if !pressed {
            self.cancel();
            return;
        }
        if let Some(value) = self.value {
            if layout.slider_hit.contains(x, y) {
                self.drag = Some(Drag::Volume(Trackbar::new(layout.slider_hit, true).press(
                    100 - i32::from(value.left.max(value.right)),
                    100,
                    (x, y),
                )));
                self.motion(layout, x, y);
            } else if let Some(bar) = layout.balance_bar.filter(|bar| bar.contains(x, y)) {
                self.drag = Some(Drag::Balance(Trackbar::new(bar, false).press(
                    self.balance + 100,
                    200,
                    (x, y),
                )));
                self.motion(layout, x, y);
            } else if layout.mute_hit.contains(x, y) {
                self.change(value.left.max(value.right), !value.muted);
            }
        }
    }
    pub fn paint(&self, p: &mut Painter<'_>, layout: &Layout) {
        let percent = self.value.map(|v| v.left.max(v.right));
        p.text_centered(
            layout.label,
            layout.title,
            Ink::Color(C::ButtonText),
            layout.title_bold,
        );
        if let Some((label, bar)) = layout.balance_label.zip(layout.balance_bar) {
            p.text(label.0, label.1, "平衡:", Ink::Color(C::ButtonText), false);
            p.trackbar(bar, false, (self.balance + 100, 200), 10);
        }
        if let Some((x, y)) = layout.volume_label {
            p.text(x, y, "音量:", Ink::Color(C::ButtonText), false);
        }
        p.trackbar(
            layout.slider_hit,
            true,
            (100 - i32::from(percent.unwrap_or(0)), 100),
            10,
        );
        let (check_x, check_y) = layout.check;
        p.checkbox(
            check_x,
            check_y,
            self.value.is_some_and(|value| value.muted),
            self.value.is_none(),
        );
        p.label_fit(
            check_x + 20,
            check_y,
            layout.mute_hit.right() - (check_x + 20),
            layout.mute_label,
            if self.value.is_some() {
                Ink::Color(C::ButtonText)
            } else {
                Ink::Disabled
            },
        );
        if let (false, Some((x, y))) = (self.message.is_empty(), layout.message) {
            p.text(x, y, self.message, Ink::Disabled, false);
        }
    }
}

/// 一个整数点是否落在矩形内（指针坐标带小数，所以接受 f64）。
pub trait Contains {
    fn contains(&self, x: f64, y: f64) -> bool;
}
impl Contains for Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && x < f64::from(self.right())
            && y >= f64::from(self.y)
            && y < f64::from(self.bottom())
    }
}

/// 音量控件在某个表面里的几何：同一套状态既画在"音量控制"窗口里，也画在托盘弹出的小面板里。
pub struct Layout {
    pub label: Rect,
    pub title: &'static str,
    pub title_bold: bool,
    /// 竖向音量滑块条的矩形（兼作命中区）。
    pub slider_hit: Rect,
    pub balance_label: Option<(i32, i32)>,
    pub balance_bar: Option<Rect>,
    pub volume_label: Option<(i32, i32)>,
    pub mute_hit: Rect,
    /// 复选框左上角；静音文字在其右侧。
    pub check: (i32, i32),
    pub mute_label: &'static str,
    pub message: Option<(i32, i32)>,
}

/// 托盘面板的版面常量（逻辑像素）：边距、复选框到标签的偏移、总高度。
const POPUP_PADDING: i32 = 8;
const POPUP_CHECK_INDENT: i32 = 8;
const POPUP_LABEL_OFFSET: i32 = 20;
const POPUP_HEIGHT: i32 = 150;
const POPUP_TITLE: &str = "音量";
const POPUP_MUTE_LABEL: &str = "静音(M)";

impl Layout {
    /// "音量控制"窗口：`client` 是窗口客户区（扣除标题栏与菜单栏之后），单列 Windows 2000 混音器。
    pub fn dialog(client: Rect) -> Self {
        let column = Rect::new(client.x + 8, client.y + 6, client.width - 16, 250);
        let bar = Rect::new(column.x + (column.width - 40) / 2, column.y + 88, 40, 130);
        Self {
            label: Rect::new(column.x, column.y, column.width, 20),
            title: "音量控制",
            title_bold: true,
            slider_hit: bar,
            balance_label: Some((column.x + 4, column.y + 28)),
            balance_bar: Some(Rect::new(column.x + 4, column.y + 44, column.width - 8, 30)),
            volume_label: Some((column.x + 4, column.y + 78)),
            mute_hit: Rect::new(column.x, column.y + 224, column.width, 22),
            check: (column.x + 4, column.y + 228),
            mute_label: "全部静音(M)",
            message: Some((client.x + 8, column.bottom() + 6)),
        }
    }

    /// 托盘面板的尺寸（逻辑像素），由内容量出，换字体或语言后自动跟着变。
    ///
    /// # Parameters
    ///
    /// - `text_width`: 返回一段文字的逻辑像素宽度。
    ///
    /// 宽度取“静音一行”（左边距 + 复选框与间隔 + 标签 + 右边距）与标题行中较宽者；高度由滑块轨道与上下两行决定。
    pub fn popup_size(mut text_width: impl FnMut(&str) -> i32) -> (i32, i32) {
        let mute_row =
            POPUP_CHECK_INDENT + POPUP_LABEL_OFFSET + text_width(POPUP_MUTE_LABEL) + POPUP_PADDING;
        let title_row = 2 * POPUP_PADDING + text_width(POPUP_TITLE);
        (mute_row.max(title_row), POPUP_HEIGHT)
    }

    /// 托盘弹出的面板，左上角在 `panel`。
    pub fn popup(panel: Rect) -> Self {
        let bar = Rect::new(panel.x + (panel.width - 40) / 2, panel.y + 22, 40, 96);
        Self {
            label: Rect::new(panel.x + 4, panel.y + 4, panel.width - 8, 16),
            title: POPUP_TITLE,
            title_bold: false,
            slider_hit: bar,
            balance_label: None,
            balance_bar: None,
            volume_label: None,
            mute_hit: Rect::new(panel.x + 4, panel.y + 122, panel.width - 8, 22),
            check: (panel.x + POPUP_CHECK_INDENT, panel.y + 126),
            mute_label: POPUP_MUTE_LABEL,
            message: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slider_maps_the_whole_track_to_zero_through_one_hundred_in_both_layouts() {
        let dialog = Layout::dialog(Rect::new(3, 22, 140, 280));
        let popup = Layout::popup(Rect::new(100, 200, 64, 150));
        for layout in [&dialog, &popup] {
            let bar = Trackbar::new(layout.slider_hit, true);
            let x = f64::from(layout.slider_hit.right() - 1);
            for (y, expected) in [
                (f64::from(layout.slider_hit.y + 5), 0),
                (f64::from(layout.slider_hit.bottom() - 6), 100),
                (-50., 0),
                (5000., 100),
            ] {
                let grab = bar.press(50, 100, (x, y));
                assert_eq!(grab.position(bar, 100, (x, y)), expected);
            }
        }
    }

    #[test]
    fn popup_hit_areas_stay_inside_the_panel() {
        let panel = Rect::new(100, 200, 64, 150);
        let layout = Layout::popup(panel);
        for hit in [layout.slider_hit, layout.mute_hit, layout.label] {
            assert!(hit.x >= panel.x && hit.right() <= panel.right());
            assert!(hit.y >= panel.y && hit.bottom() <= panel.bottom());
        }
    }
}
