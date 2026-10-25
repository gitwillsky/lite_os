//! 音量状态与布局：音量控制窗口与托盘面板共用；只展示 ALSA 快照；每次操作后与外部更新均重新读取，不复制可写 mixer owner。
extern crate liteos_runtime as _;
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{Configuration, Ink, Painter};
use linux_uapi::alsa::control::{MasterControl, MasterVolume};
use std::time::{Duration, Instant};

pub struct Volume {
    control: Option<MasterControl>,
    pub value: Option<MasterVolume>,
    pub message: &'static str,
    // 仅 slider 按下至 release/leave/focus-loss 期间有效，防止普通悬停修改 Master。
    dragging: bool,
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
                    dragging: false,
                    next_refresh: Instant::now(),
                };
                state.refresh();
                state
            }
            Err(error) => Self {
                control: None,
                value: None,
                dragging: false,
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
    pub fn change(&mut self, percent: u8, muted: bool) {
        if self.value.is_some_and(|value| {
            value.left == percent && value.right == percent && value.muted == muted
        }) {
            return;
        }
        if let Some(control) = &self.control
            && let Err(error) = control.set(percent, muted)
        {
            eprintln!("desktop: ALSA Master write: {error}");
        }
        self.refresh();
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
        self.dragging
    }
    pub fn cancel(&mut self) {
        self.dragging = false;
    }
    /// 滑块位置对应的百分比；超出轨道时夹在 0..=100。
    fn percent_at(layout: &Layout, y: f64) -> u8 {
        ((f64::from(layout.track_top + layout.track_len) - y) * 100. / f64::from(layout.track_len))
            .clamp(0., 100.) as u8
    }
    pub fn motion(&mut self, layout: &Layout, y: f64) {
        if self.dragging
            && let Some(value) = self.value
        {
            self.change(Self::percent_at(layout, y), value.muted);
        }
    }
    pub fn button(&mut self, layout: &Layout, x: f64, y: f64, pressed: bool) {
        if !pressed {
            self.cancel();
            return;
        }
        if let Some(value) = self.value {
            if layout.slider_hit.contains(x, y) {
                self.dragging = true;
                self.change(Self::percent_at(layout, y), value.muted);
            } else if layout.mute_hit.contains(x, y) {
                self.change(value.left.max(value.right), !value.muted);
            }
        }
    }
    pub fn paint(&self, p: &mut Painter<'_>, layout: &Layout) {
        let percent = self.value.map(|v| v.left.max(v.right));
        p.text_centered(
            layout.label,
            &percent.map_or("音量".into(), |value| format!("音量 {value}%")),
            Ink::Color(C::ButtonText),
            false,
        );
        let x = layout.center;
        p.fill(
            Rect::new(x - 2, layout.track_top, 4, layout.track_len + 1),
            C::ButtonShadow,
        );
        if let Some(value) = percent {
            let y = layout.track_top + layout.track_len - i32::from(value) * layout.track_len / 100;
            p.fill(Rect::new(x - 10, y - 5, 20, 10), C::ButtonFace);
            p.outline(Rect::new(x - 10, y - 5, 20, 10), C::ButtonText);
        }
        let (check_x, check_y) = layout.check;
        p.checkbox(
            check_x,
            check_y,
            self.value.is_some_and(|value| value.muted),
            self.value.is_none(),
        );
        p.text(
            check_x + 20,
            check_y,
            "静音",
            if self.value.is_some() {
                Ink::Color(C::ButtonText)
            } else {
                Ink::Disabled
            },
            false,
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
    pub center: i32,
    pub track_top: i32,
    pub track_len: i32,
    pub slider_hit: Rect,
    pub mute_hit: Rect,
    /// 复选框左上角；"静音"文字在其右侧。
    pub check: (i32, i32),
    pub message: Option<(i32, i32)>,
}

impl Layout {
    /// "音量控制"窗口（客户区整个 surface 宽度 `c.width`）。
    pub fn dialog(c: Configuration) -> Self {
        Self {
            label: Rect::new(15, 42, c.width - 30, 25),
            center: c.width / 2,
            track_top: 80,
            track_len: 120,
            slider_hit: Rect::new(c.width / 2 - 20, 75, 40, 131),
            mute_hit: Rect::new(20, 216, c.width - 40, 27),
            check: (25, 220),
            message: Some((12, 250)),
        }
    }

    /// 托盘弹出的 64 × 150 面板，左上角在 `panel`。
    pub fn popup(panel: Rect) -> Self {
        Self {
            label: Rect::new(panel.x + 4, panel.y + 6, panel.width - 8, 18),
            center: panel.x + panel.width / 2,
            track_top: panel.y + 32,
            track_len: 80,
            slider_hit: Rect::new(panel.x + panel.width / 2 - 14, panel.y + 27, 28, 91),
            mute_hit: Rect::new(panel.x + 4, panel.y + 122, panel.width - 8, 22),
            check: (panel.x + 8, panel.y + 126),
            message: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slider_maps_the_whole_track_to_zero_through_one_hundred_in_both_layouts() {
        let dialog = Layout::dialog(Configuration {
            width: 260,
            height: 320,
            scale: 1,
            active: true,
            maximized: false,
            fullscreen: false,
            resizing: false,
        });
        let popup = Layout::popup(Rect::new(100, 200, 64, 150));
        for layout in [&dialog, &popup] {
            assert_eq!(Volume::percent_at(layout, f64::from(layout.track_top)), 100);
            assert_eq!(
                Volume::percent_at(layout, f64::from(layout.track_top + layout.track_len)),
                0
            );
            assert_eq!(Volume::percent_at(layout, -50.), 100);
            assert_eq!(Volume::percent_at(layout, 5000.), 0);
            let middle = f64::from(layout.track_top + layout.track_len / 2);
            assert_eq!(Volume::percent_at(layout, middle), 50);
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
