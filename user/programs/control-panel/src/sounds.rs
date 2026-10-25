//! "声音和多媒体 属性"：声音事件与对应文件（可试听），以及音频播放设备与音量控制入口。
use crate::{Icons, Page, inside};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{ButtonState, Ink, Painter};
use std::{path::Path, process::Command};

/// 一个声音事件：显示名与 `/usr/share/liteos/sounds/` 下的文件。
struct Event {
    name: &'static str,
    file: &'static str,
}

/// 与桌面、资源管理器实际播放的事件一致；"默认声音"系列都映射到同一个提示音。
const EVENTS: [Event; 8] = [
    Event {
        name: "默认声音",
        file: "chord.wav",
    },
    Event {
        name: "严重错误",
        file: "chord.wav",
    },
    Event {
        name: "感叹号",
        file: "chord.wav",
    },
    Event {
        name: "星号",
        file: "chord.wav",
    },
    Event {
        name: "询问",
        file: "chord.wav",
    },
    Event {
        name: "登录 LiteOS",
        file: "logon.wav",
    },
    Event {
        name: "注销 LiteOS",
        file: "logoff.wav",
    },
    Event {
        name: "清空回收站",
        file: "recycle.wav",
    },
];
const SOUND_DIRECTORY: &str = "/usr/share/liteos/sounds";
const PLAYBACK_NODE: &str = "/dev/snd/pcmC0D0p";
const ROW: i32 = 17;

pub struct Sounds {
    selected: usize,
}

impl Sounds {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    fn list(page: Rect) -> Rect {
        Rect::new(page.x + 12, page.y + 78, page.width - 24, 8 * ROW + 4)
    }

    fn preview_button(page: Rect) -> Rect {
        Rect::new(
            page.right() - 12 - 75,
            page.y + 78 + 8 * ROW + 4 + 38,
            75,
            23,
        )
    }

    fn volume_button(page: Rect) -> Rect {
        Rect::new(page.right() - 12 - 85, page.y + 12 + 80 + 8, 85, 23)
    }

    fn device_available() -> bool {
        Path::new(PLAYBACK_NODE).exists()
    }

    fn paint_sounds(&self, p: &mut Painter<'_>, page: Rect) {
        let ink = Ink::Color(C::ButtonText);
        p.text(
            page.x + 12,
            page.y + 12,
            "将声音指定给程序事件。点击事件即可试听对应的声音。",
            ink,
            false,
        );
        p.text(page.x + 12, page.y + 60, "声音事件(E):", ink, false);
        let list = Self::list(page);
        let inner = p.edge(list, Edge::Sunken);
        p.fill(inner, C::Window);
        let present = |file: &str| Path::new(SOUND_DIRECTORY).join(file).exists();
        p.clipped(inner, |p| {
            p.text(
                inner.x + 4,
                inner.y + 2,
                "LiteOS",
                Ink::Color(C::WindowText),
                true,
            );
            for (index, event) in EVENTS.iter().enumerate() {
                let y = inner.y + ROW * (1 + index as i32);
                let hot = index == self.selected;
                let text_x = inner.x + 28;
                let width = (p.text_width(event.name, false) + p.scale() - 1) / p.scale();
                if hot {
                    p.fill(Rect::new(text_x - 2, y, width + 4, ROW - 1), C::Hilight);
                }
                p.text(
                    text_x,
                    y + 1,
                    event.name,
                    Ink::Color(if hot { C::HilightText } else { C::WindowText }),
                    false,
                );
                // 有声音文件的事件在名称左侧画一个小喇叭标记。
                if present(event.file) {
                    p.fill(Rect::new(inner.x + 16, y + 6, 3, 5), C::WindowText);
                    p.fill(Rect::new(inner.x + 19, y + 4, 2, 9), C::WindowText);
                }
            }
        });
        let below = list.bottom() + 8;
        p.text(page.x + 12, below + 4, "名称(N):", ink, false);
        let field = Rect::new(page.x + 70, below, page.width - 70 - 12 - 75 - 8, 21);
        let inner = p.edge(field, Edge::Sunken);
        p.fill(inner, C::ButtonFace);
        p.text(
            inner.x + 3,
            inner.y + 2,
            EVENTS[self.selected].file,
            Ink::Color(C::WindowText),
            false,
        );
        p.push_button(
            Self::preview_button(page),
            "试听(P)",
            ButtonState {
                disabled: !present(EVENTS[self.selected].file),
                ..Default::default()
            },
        );
    }

    fn paint_audio(&self, p: &mut Painter<'_>, page: Rect) {
        let ink = Ink::Color(C::ButtonText);
        let group = Rect::new(page.x + 12, page.y + 12, page.width - 24, 80);
        p.group_box(group, "声音播放");
        p.text(group.x + 12, group.y + 24, "默认设备(D):", ink, false);
        let field = Rect::new(group.x + 12, group.y + 40, group.width - 24, 21);
        let inner = p.edge(field, Edge::Sunken);
        p.fill(inner, C::ButtonFace);
        let name = if Self::device_available() {
            "LiteOS Playback"
        } else {
            "没有可用的音频输出"
        };
        p.text(
            inner.x + 3,
            inner.y + 2,
            name,
            Ink::Color(C::WindowText),
            false,
        );
        p.push_button(
            Self::volume_button(page),
            "音量(V)...",
            ButtonState {
                disabled: !Self::device_available(),
                ..Default::default()
            },
        );
        let record = Rect::new(page.x + 12, page.y + 12 + 80 + 8 + 36, page.width - 24, 50);
        p.group_box(record, "录音");
        p.text(
            record.x + 12,
            record.y + 26,
            "没有可用的录音设备。",
            ink,
            false,
        );
        let midi = Rect::new(record.x, record.bottom() + 8, record.width, 50);
        p.group_box(midi, "MIDI 音乐播放");
        p.text(
            midi.x + 12,
            midi.y + 26,
            "没有可用的 MIDI 设备。",
            ink,
            false,
        );
    }
}

impl Page for Sounds {
    fn title(&self) -> &'static str {
        "声音和多媒体 属性"
    }
    fn tabs(&self) -> &'static [&'static str] {
        &["声音", "音频"]
    }
    fn icon(&self) -> &'static str {
        "sound-volume"
    }
    fn size(&self) -> (i32, i32) {
        (400, 420)
    }
    fn paint(&self, p: &mut Painter<'_>, page: Rect, tab: usize, _now: i64, _icons: &Icons) {
        if tab == 0 {
            self.paint_sounds(p, page);
        } else {
            self.paint_audio(p, page);
        }
    }
    fn click(&mut self, page: Rect, tab: usize, point: (i32, i32)) -> bool {
        if tab == 0 {
            let list = Self::list(page).inset(2);
            if inside(list, point) {
                let row = (point.1 - list.y) / ROW - 1;
                if (0..EVENTS.len() as i32).contains(&row) {
                    self.selected = row as usize;
                    let _ = sound::play(EVENTS[self.selected].file);
                }
                return true;
            }
            if inside(Self::preview_button(page), point) {
                let _ = sound::play(EVENTS[self.selected].file);
            }
        } else if inside(Self::volume_button(page), point) && Self::device_available() {
            let _ = Command::new("/bin/sndvol")
                .env_remove("WAYLAND_SOCKET")
                .spawn();
        }
        false
    }
}
