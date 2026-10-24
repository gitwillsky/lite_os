//! 音量窗口只展示 ALSA 快照；每次操作后与外部更新均重新读取，不复制可写 mixer owner。
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
    pub fn cancel(&mut self) {
        self.dragging = false;
    }
    pub fn motion(&mut self, y: f64) {
        if self.dragging
            && let Some(value) = self.value
        {
            self.change(
                ((200. - y) * 100. / 120.).clamp(0., 100.) as u8,
                value.muted,
            );
        }
    }
    pub fn button(&mut self, x: f64, y: f64, c: Configuration, pressed: bool) {
        if !pressed {
            self.cancel();
            return;
        }
        if let Some(value) = self.value {
            if x >= f64::from(c.width / 2 - 20)
                && x < f64::from(c.width / 2 + 20)
                && (75.0..=205.0).contains(&y)
            {
                self.dragging = true;
                let percent = ((200. - y) * 100. / 120.).clamp(0., 100.) as u8;
                self.change(percent, value.muted);
            } else if x >= 20. && x < f64::from(c.width - 20) && (216.0..243.0).contains(&y) {
                self.change(value.left.max(value.right), !value.muted);
            }
        }
    }
    pub fn paint(&self, p: &mut Painter<'_>, c: Configuration) {
        let percent = self.value.map(|v| v.left.max(v.right));
        p.text_centered(
            Rect::new(15, 42, c.width - 30, 25),
            &percent.map_or("音量".into(), |value| format!("音量 {value}%")),
            Ink::Color(C::ButtonText),
            false,
        );
        let x = c.width / 2;
        p.fill(Rect::new(x - 2, 80, 4, 121), C::ButtonShadow);
        if let Some(value) = percent {
            let y = 200 - i32::from(value) * 120 / 100;
            p.fill(Rect::new(x - 10, y - 5, 20, 10), C::ButtonFace);
            p.outline(Rect::new(x - 10, y - 5, 20, 10), C::ButtonText);
        }
        p.checkbox(
            25,
            220,
            self.value.is_some_and(|value| value.muted),
            self.value.is_none(),
        );
        p.text(
            45,
            220,
            "静音",
            if self.value.is_some() {
                Ink::Color(C::ButtonText)
            } else {
                Ink::Disabled
            },
            false,
        );
        if !self.message.is_empty() {
            p.text(12, 250, self.message, Ink::Disabled, false);
        }
    }
}
