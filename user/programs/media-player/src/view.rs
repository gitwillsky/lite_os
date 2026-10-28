//! 窗口控件与菜单：全部使用共享控件，视频帧经 Canvas 的媒体帧入口显示。
use crate::engine::{Changes, Engine, Repeat, State, TrackKind, clock};
use liteos_ui::{
    Draw, Layout, Menu, MenuItem, Range, Rect, Widget, WidgetKind as Kind, WidgetSpec, Window,
    state,
};
use std::cell::Cell;

/// 菜单与按钮触发的动作；轨道、速度等带参数的菜单项在 `Action::decode` 里还原。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Open,
    OpenUrl,
    AddToPlaylist,
    Screenshot,
    CloseFile,
    Exit,
    Pause,
    Stop,
    Previous,
    Next,
    Seek(f64),
    FrameBack,
    FrameStep,
    PreviousChapter,
    NextChapter,
    Speed(f64),
    /// 在当前速度上乘一个系数（`[` / `]` 快捷键）。
    SpeedBy(f64),
    LoopAb,
    Repeat(Repeat),
    Fullscreen,
    Aspect(&'static str),
    Rotate(i32),
    FastScaling,
    Track(TrackKind, Option<i64>),
    VolumeStep(f64),
    Mute,
    AudioDelay(f64),
    AddSubtitle,
    SubtitleDelay(f64),
    SubtitleScale(f64),
    TogglePlaylist,
    Info,
    About,
}

const SPEEDS: [f64; 8] = [0.25, 0.5, 0.75, 1., 1.25, 1.5, 2., 4.];
const ASPECTS: [(&str, &str); 5] = [
    ("自动", "-1"),
    ("16:9", "16:9"),
    ("4:3", "4:3"),
    ("21:9", "21:9"),
    ("1:1", "1:1"),
];
const ROTATIONS: [i32; 4] = [0, 90, 180, 270];

// 菜单项编号：固定项占 1..100；带参数的区间各占一段，`decode` 反查。
const OPEN: u64 = 1;
const OPEN_URL: u64 = 2;
const ADD: u64 = 3;
const SHOT: u64 = 4;
const CLOSE_FILE: u64 = 5;
const EXIT: u64 = 6;
const PAUSE: u64 = 10;
const STOP: u64 = 11;
const PREV: u64 = 12;
const NEXT: u64 = 13;
const BACK5: u64 = 14;
const FWD5: u64 = 15;
const BACK60: u64 = 16;
const FWD60: u64 = 17;
const FRAME_BACK: u64 = 18;
const FRAME_STEP: u64 = 19;
const CHAPTER_BACK: u64 = 20;
const CHAPTER_FWD: u64 = 21;
const SPEED_MENU: u64 = 30;
const SPEED_BASE: u64 = 31;
const AB: u64 = 40;
const REPEAT_MENU: u64 = 45;
const REPEAT_OFF: u64 = 46;
const REPEAT_ONE: u64 = 47;
const REPEAT_ALL: u64 = 48;
const FULLSCREEN: u64 = 50;
const ASPECT_MENU: u64 = 51;
const ASPECT_BASE: u64 = 52;
const ROTATE_MENU: u64 = 60;
const ROTATE_BASE: u64 = 61;
const FAST: u64 = 65;
const VIDEO_MENU: u64 = 70;
const AUDIO_MENU: u64 = 80;
const VOL_UP: u64 = 81;
const VOL_DOWN: u64 = 82;
const MUTE: u64 = 83;
const AUDIO_SLOWER: u64 = 84;
const AUDIO_FASTER: u64 = 85;
const SUB_MENU: u64 = 90;
const ADD_SUB: u64 = 91;
const SUB_EARLIER: u64 = 92;
const SUB_LATER: u64 = 93;
const SUB_SMALLER: u64 = 94;
const SUB_LARGER: u64 = 95;
const PLAYLIST: u64 = 100;
const INFO: u64 = 101;
const ABOUT: u64 = 110;
const VIDEO_TRACK: u64 = 1000;
const AUDIO_TRACK: u64 = 2000;
const SUB_TRACK: u64 = 3000;
/// 轨道项的“关闭”（`no`）偏移；轨道 id 从 1 起，所以 0 不会与真实轨道冲突。
const NO_TRACK: u64 = 0;

impl Action {
    /// 把菜单项编号还原成动作；未知编号返回 None。
    pub fn decode(id: u64) -> Option<Self> {
        let in_range = |base: u64, count: usize| {
            id.checked_sub(base)
                .filter(|offset| (*offset as usize) < count)
        };
        let track = |base: u64, kind| {
            id.checked_sub(base)
                .filter(|offset| *offset < 1000)
                .map(|offset| Self::Track(kind, (offset != NO_TRACK).then_some(offset as i64)))
        };
        Some(match id {
            OPEN => Self::Open,
            OPEN_URL => Self::OpenUrl,
            ADD => Self::AddToPlaylist,
            SHOT => Self::Screenshot,
            CLOSE_FILE => Self::CloseFile,
            EXIT => Self::Exit,
            PAUSE => Self::Pause,
            STOP => Self::Stop,
            PREV => Self::Previous,
            NEXT => Self::Next,
            BACK5 => Self::Seek(-5.),
            FWD5 => Self::Seek(5.),
            BACK60 => Self::Seek(-60.),
            FWD60 => Self::Seek(60.),
            FRAME_BACK => Self::FrameBack,
            FRAME_STEP => Self::FrameStep,
            CHAPTER_BACK => Self::PreviousChapter,
            CHAPTER_FWD => Self::NextChapter,
            AB => Self::LoopAb,
            REPEAT_OFF => Self::Repeat(Repeat::Off),
            REPEAT_ONE => Self::Repeat(Repeat::One),
            REPEAT_ALL => Self::Repeat(Repeat::All),
            FULLSCREEN => Self::Fullscreen,
            FAST => Self::FastScaling,
            VOL_UP => Self::VolumeStep(5.),
            VOL_DOWN => Self::VolumeStep(-5.),
            MUTE => Self::Mute,
            AUDIO_SLOWER => Self::AudioDelay(-0.1),
            AUDIO_FASTER => Self::AudioDelay(0.1),
            ADD_SUB => Self::AddSubtitle,
            SUB_EARLIER => Self::SubtitleDelay(-0.1),
            SUB_LATER => Self::SubtitleDelay(0.1),
            SUB_SMALLER => Self::SubtitleScale(-0.1),
            SUB_LARGER => Self::SubtitleScale(0.1),
            PLAYLIST => Self::TogglePlaylist,
            INFO => Self::Info,
            ABOUT => Self::About,
            _ => {
                return in_range(SPEED_BASE, SPEEDS.len())
                    .map(|index| Self::Speed(SPEEDS[index as usize]))
                    .or_else(|| {
                        in_range(ASPECT_BASE, ASPECTS.len())
                            .map(|index| Self::Aspect(ASPECTS[index as usize].1))
                    })
                    .or_else(|| {
                        in_range(ROTATE_BASE, ROTATIONS.len())
                            .map(|index| Self::Rotate(ROTATIONS[index as usize]))
                    })
                    .or_else(|| track(VIDEO_TRACK, TrackKind::Video))
                    .or_else(|| track(AUDIO_TRACK, TrackKind::Audio))
                    .or_else(|| track(SUB_TRACK, TrackKind::Subtitle));
            }
        })
    }
}

fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}

/// 窗口里所有控件的 owner：`Widget` 析构会删除运行库里的节点，所以每个控件都存放在这里。
pub struct View {
    pub window: Window,
    pub menu: Widget,
    pub video: Widget,
    pub list: Widget,
    pub seek: Widget,
    pub volume: Widget,
    pub previous: Widget,
    pub play: Widget,
    pub stop: Widget,
    pub next: Widget,
    time: Widget,
    status: Widget,
    side: Widget,
    controls: Widget,
    _layout: Vec<Widget>,
    pub fullscreen: Cell<bool>,
    pub side_visible: Cell<bool>,
    pub fast_scaling: Cell<bool>,
    /// 设备像素与逻辑像素的比，来自最近一次 Configure。
    pub scale: Cell<i32>,
    /// 上一次刷新滑块与时间标签的时刻；位置事件每帧到达，界面只需约 10 Hz。
    last_position: Cell<Option<std::time::Instant>>,
}

impl View {
    pub fn new(window: &Window) -> liteos_ui::Result<Self> {
        let mut spec = WidgetSpec::new(Kind::Column, "");
        spec.layout = Layout {
            grow: 1,
            gap: 0,
            ..Default::default()
        };
        let root = window.widget(spec)?;
        let menu = child(&root, Kind::MenuBar, "", Layout::default())?;
        let body = child(
            &root,
            Kind::Row,
            "",
            Layout {
                grow: 1,
                gap: 0,
                ..Default::default()
            },
        )?;
        let video = child(
            &body,
            Kind::Canvas,
            "",
            Layout {
                grow: 1,
                min_width: 240,
                min_height: 140,
                ..Default::default()
            },
        )?;
        let side = child(
            &body,
            Kind::Column,
            "",
            Layout {
                width: 200,
                gap: 0,
                ..Default::default()
            },
        )?;
        side.set_state(state::HIDDEN)?;
        let list = child(
            &side,
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        list.set_state(state::CLIENT_EDGE)?;
        let controls = child(
            &root,
            Kind::Column,
            "",
            Layout {
                padding: 6,
                gap: 4,
                ..Default::default()
            },
        )?;
        let seek = child(
            &controls,
            Kind::Slider,
            "",
            Layout {
                height: 25,
                ..Default::default()
            },
        )?;
        let row = child(
            &controls,
            Kind::Row,
            "",
            Layout {
                height: 25,
                gap: 4,
                ..Default::default()
            },
        )?;
        let button = |text: &str, width: i32| {
            child(
                &row,
                Kind::Button,
                text,
                Layout {
                    width,
                    ..Default::default()
                },
            )
        };
        let previous = button("上一个", 56)?;
        let play = button("播放", 56)?;
        let stop = button("停止", 56)?;
        let next = button("下一个", 56)?;
        let time = child(
            &row,
            Kind::Label,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let volume_label = child(
            &row,
            Kind::Label,
            "音量",
            Layout {
                width: 34,
                ..Default::default()
            },
        )?;
        let volume = child(
            &row,
            Kind::Slider,
            "",
            Layout {
                width: 110,
                ..Default::default()
            },
        )?;
        let status = child(&root, Kind::Status, "", Layout::default())?;
        volume.set_range(Range {
            minimum: 0,
            maximum: 130,
            value: 100,
            page: 10,
            step: 5,
            vertical: 0,
        })?;
        Ok(Self {
            window: window.clone(),
            menu,
            video,
            list,
            seek,
            volume,
            previous,
            play,
            stop,
            next,
            time,
            status,
            side,
            controls,
            _layout: vec![root, body, row, volume_label],
            fullscreen: Cell::new(false),
            side_visible: Cell::new(false),
            fast_scaling: Cell::new(false),
            scale: Cell::new(1),
            last_position: Cell::new(None),
        })
    }

    /// 按变化的部分把引擎状态同步到控件；位置类更新限频，避免每帧都重排滑块与标签。
    pub fn apply(&self, engine: &mut Engine, changes: Changes) -> liteos_ui::Result<()> {
        if changes.controls {
            self.controls_state(&engine.state)?;
        }
        if changes.position {
            let due = self
                .last_position
                .get()
                .is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(100));
            if due || changes.controls {
                self.position(&engine.state)?;
                self.last_position.set(Some(std::time::Instant::now()));
            }
        }
        if changes.menus {
            self.menus(&engine.state)?;
        }
        if changes.playlist {
            self.playlist(&engine.state)?;
        }
        if changes.frame {
            self.render(engine)?;
        }
        Ok(())
    }

    fn controls_state(&self, state_: &State) -> liteos_ui::Result<()> {
        let title = if state_.title.is_empty() {
            "媒体播放机".to_owned()
        } else {
            format!("{} - 媒体播放机", state_.title)
        };
        self.window.set_title(&title)?;
        let open = !state_.idle;
        let disabled = |enabled: bool| if enabled { 0 } else { state::DISABLED };
        self.play.set_text(if open && !state_.paused {
            "暂停"
        } else {
            "播放"
        })?;
        self.play.set_state(disabled(open))?;
        self.stop.set_state(disabled(open))?;
        self.previous
            .set_state(disabled(state_.playlist.len() > 1))?;
        self.next.set_state(disabled(state_.playlist.len() > 1))?;
        self.seek
            .set_state(disabled(open && state_.duration > 0.))?;
        self.volume.set_range(Range {
            minimum: 0,
            maximum: 130,
            value: state_.volume.round() as i32,
            page: 10,
            step: 5,
            vertical: 0,
        })?;
        let mut parts = vec![if state_.message.is_empty() {
            match (open, state_.paused, state_.buffering) {
                (false, ..) => "就绪".to_owned(),
                (true, _, true) => "正在缓冲…".to_owned(),
                (true, true, _) => "已暂停".to_owned(),
                _ => "正在播放".to_owned(),
            }
        } else {
            state_.message.clone()
        }];
        parts.push(format!("速度 {}x", state_.speed));
        parts.push(match (state_.loop_a, state_.loop_b) {
            (Some(a), Some(b)) => format!("A-B {} – {}", clock(a), clock(b)),
            (Some(a), None) => format!("A {}", clock(a)),
            _ => String::new(),
        });
        self.status.set_text(&parts.join("\t"))
    }

    fn position(&self, state_: &State) -> liteos_ui::Result<()> {
        self.time.set_text(&format!(
            "{} / {}",
            clock(state_.position),
            clock(state_.duration)
        ))?;
        let permille = if state_.duration > 0. {
            (state_.position / state_.duration * 1000.).clamp(0., 1000.) as i32
        } else {
            0
        };
        self.seek.set_range(Range {
            minimum: 0,
            maximum: 1000,
            value: permille,
            page: 50,
            step: 10,
            vertical: 0,
        })
    }

    fn playlist(&self, state_: &State) -> liteos_ui::Result<()> {
        let rows: Vec<(u64, &str)> = state_
            .playlist
            .iter()
            .enumerate()
            .map(|(index, entry)| (index as u64 + 1, entry.label.as_str()))
            .collect();
        self.list.set_rows(&rows)?;
        if let Some(index) = state_.playlist.iter().position(|entry| entry.current) {
            self.list.select_rows(&[index as u64 + 1])?;
        }
        Ok(())
    }

    /// 视频画布的设备像素尺寸：逻辑矩形乘以窗口 scale。
    pub fn canvas_pixels(&self) -> liteos_ui::Result<Option<(u32, u32)>> {
        let area = self.video.geometry()?;
        let scale = self.scale.get().max(1);
        Ok((area.width > 0 && area.height > 0)
            .then(|| ((area.width * scale) as u32, (area.height * scale) as u32)))
    }

    /// 渲染当前帧；空闲时画黑底并清除旧帧。
    pub fn render(&self, engine: &mut Engine) -> liteos_ui::Result<()> {
        let Some((width, height)) = self.canvas_pixels()? else {
            return Ok(());
        };
        if engine.state.idle {
            self.video.set_frame(0, 0, &[])?;
            let area = self.video.geometry()?;
            return self.video.set_drawing(&[Draw {
                kind: 1,
                rect: Rect::new(0, 0, area.width, area.height),
                color: 0,
                bold: false,
                text: "",
            }]);
        }
        self.video.set_drawing(&[])?;
        match engine.render(width, height) {
            Ok(pixels) => self.video.set_frame(width, height, pixels),
            Err(error) => {
                engine.state.message = format!("渲染失败：{error}");
                self.controls_state(&engine.state)
            }
        }
    }

    pub fn show_side(&self, visible: bool) -> liteos_ui::Result<()> {
        self.side_visible.set(visible);
        self.side.set_state(if visible { 0 } else { state::HIDDEN })
    }

    /// 全屏时隐藏菜单、控制条与状态栏，只留视频；退出全屏恢复。
    pub fn set_fullscreen(&self, on: bool) -> liteos_ui::Result<()> {
        self.fullscreen.set(on);
        let hidden = if on { state::HIDDEN } else { 0 };
        self.menu.set_state(hidden)?;
        self.controls.set_state(hidden)?;
        self.status.set_state(hidden)?;
        self.side.set_state(if on || !self.side_visible.get() {
            state::HIDDEN
        } else {
            0
        })?;
        self.window.set_fullscreen(on)
    }

    fn menus(&self, state_: &State) -> liteos_ui::Result<()> {
        struct Item {
            id: u64,
            label: String,
            shortcut: &'static str,
            flags: u32,
            parent: u64,
        }
        const DISABLED: u32 = 1;
        const CHECKED: u32 = 2;
        const SEPARATOR: u32 = 4;
        let item = |id, label: &str, shortcut, flags, parent| Item {
            id,
            label: label.to_owned(),
            shortcut,
            flags,
            parent,
        };
        let open = !state_.idle;
        let gate = |enabled: bool| if enabled { 0 } else { DISABLED };
        let check = |on: bool| if on { CHECKED } else { 0 };
        let tracks =
            |menu: u64, base: u64, kind: TrackKind, off: &str| {
                let mut items = vec![item(menu, off_title(kind), "", gate(open), 0)];
                let selected_any = state_
                    .tracks
                    .iter()
                    .any(|track| track.kind == kind && track.selected);
                items.push(item(base + NO_TRACK, off, "", check(!selected_any), menu));
                items.extend(state_.tracks.iter().filter(|track| track.kind == kind).map(
                    |track| {
                        item(
                            base + track.id as u64,
                            &track.label,
                            "",
                            check(track.selected),
                            menu,
                        )
                    },
                ));
                items
            };
        let file = vec![
            item(OPEN, "打开(O)...", "Ctrl+O", 0, 0),
            item(OPEN_URL, "打开网址(U)...", "", 0, 0),
            item(ADD, "添加到播放列表(A)...", "", 0, 0),
            item(SHOT, "保存截图(S)", "", SEPARATOR | gate(open), 0),
            item(CLOSE_FILE, "关闭文件(C)", "", gate(open), 0),
            item(EXIT, "退出(X)", "", SEPARATOR, 0),
        ];
        let mut play = vec![
            item(PAUSE, "播放/暂停(P)", "空格", gate(open), 0),
            item(STOP, "停止(S)", "", gate(open), 0),
            item(PREV, "上一个(V)", "P", gate(state_.playlist.len() > 1), 0),
            item(NEXT, "下一个(N)", "N", gate(state_.playlist.len() > 1), 0),
            item(BACK5, "快退 5 秒", "←", SEPARATOR | gate(open), 0),
            item(FWD5, "快进 5 秒", "→", gate(open), 0),
            item(BACK60, "快退 1 分钟", "Shift+←", gate(open), 0),
            item(FWD60, "快进 1 分钟", "Shift+→", gate(open), 0),
            item(FRAME_BACK, "上一帧", ",", gate(open), 0),
            item(FRAME_STEP, "下一帧", ".", gate(open), 0),
            item(
                CHAPTER_BACK,
                "上一章节",
                "",
                SEPARATOR | gate(state_.chapters.len() > 1),
                0,
            ),
            item(
                CHAPTER_FWD,
                "下一章节",
                "",
                gate(state_.chapters.len() > 1),
                0,
            ),
            item(SPEED_MENU, "播放速度(E)", "", SEPARATOR | gate(open), 0),
        ];
        play.extend(SPEEDS.iter().enumerate().map(|(index, speed)| {
            item(
                SPEED_BASE + index as u64,
                &format!("{speed}x"),
                "",
                check((state_.speed - speed).abs() < 1e-6),
                SPEED_MENU,
            )
        }));
        play.extend([
            item(AB, "A-B 循环(L)", "", gate(open), 0),
            item(REPEAT_MENU, "重复(R)", "", 0, 0),
            item(
                REPEAT_OFF,
                "不重复",
                "",
                check(state_.repeat == Repeat::Off),
                REPEAT_MENU,
            ),
            item(
                REPEAT_ONE,
                "单个文件循环",
                "",
                check(state_.repeat == Repeat::One),
                REPEAT_MENU,
            ),
            item(
                REPEAT_ALL,
                "列表循环",
                "",
                check(state_.repeat == Repeat::All),
                REPEAT_MENU,
            ),
        ]);
        let mut video = vec![
            item(
                FULLSCREEN,
                "全屏(F)",
                "F11",
                check(self.fullscreen.get()),
                0,
            ),
            item(ASPECT_MENU, "画面比例(A)", "", SEPARATOR | gate(open), 0),
        ];
        video.extend(
            ASPECTS.iter().enumerate().map(|(index, (name, _))| {
                item(ASPECT_BASE + index as u64, name, "", 0, ASPECT_MENU)
            }),
        );
        video.push(item(ROTATE_MENU, "旋转(R)", "", gate(open), 0));
        video.extend(ROTATIONS.iter().enumerate().map(|(index, degrees)| {
            item(
                ROTATE_BASE + index as u64,
                &format!("{degrees}°"),
                "",
                0,
                ROTATE_MENU,
            )
        }));
        video.push(item(
            FAST,
            "快速缩放(Q)",
            "",
            SEPARATOR | check(self.fast_scaling.get()),
            0,
        ));
        video.extend(tracks(VIDEO_MENU, VIDEO_TRACK, TrackKind::Video, "关闭"));
        let mut audio = tracks(AUDIO_MENU, AUDIO_TRACK, TrackKind::Audio, "关闭");
        audio.extend([
            item(VOL_UP, "增大音量", "↑", SEPARATOR, 0),
            item(VOL_DOWN, "减小音量", "↓", 0, 0),
            item(MUTE, "静音(M)", "M", check(state_.muted), 0),
            item(
                AUDIO_SLOWER,
                "音频延迟 -0.1 秒",
                "",
                SEPARATOR | gate(open),
                0,
            ),
            item(AUDIO_FASTER, "音频延迟 +0.1 秒", "", gate(open), 0),
        ]);
        let mut subtitle = tracks(SUB_MENU, SUB_TRACK, TrackKind::Subtitle, "关闭");
        subtitle.extend([
            item(ADD_SUB, "载入字幕(L)...", "", SEPARATOR | gate(open), 0),
            item(
                SUB_EARLIER,
                "字幕提前 0.1 秒",
                "",
                SEPARATOR | gate(open),
                0,
            ),
            item(SUB_LATER, "字幕延后 0.1 秒", "", gate(open), 0),
            item(SUB_SMALLER, "缩小字幕", "", gate(open), 0),
            item(SUB_LARGER, "放大字幕", "", gate(open), 0),
        ]);
        let view = vec![
            item(
                PLAYLIST,
                "播放列表(L)",
                "",
                check(self.side_visible.get()),
                0,
            ),
            item(INFO, "媒体信息(I)...", "", gate(open), 0),
        ];
        let help = vec![item(ABOUT, "关于媒体播放机(A)", "", 0, 0)];
        let owned = [
            ("文件(F)", file),
            ("播放(P)", play),
            ("视频(V)", video),
            ("音频(A)", audio),
            ("字幕(S)", subtitle),
            ("查看(W)", view),
            ("帮助(H)", help),
        ];
        let converted: Vec<Vec<MenuItem<'_>>> = owned
            .iter()
            .map(|(_, items)| {
                items
                    .iter()
                    .map(|item| MenuItem {
                        id: item.id,
                        label: &item.label,
                        shortcut: item.shortcut,
                        state: item.flags,
                        parent: item.parent,
                    })
                    .collect()
            })
            .collect();
        let menus: Vec<Menu<'_>> = owned
            .iter()
            .zip(&converted)
            .map(|((title, _), items)| Menu { title, items })
            .collect();
        self.menu.set_menus(&menus)
    }
}

fn off_title(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "视频轨道(T)",
        TrackKind::Audio => "音频轨道(T)",
        TrackKind::Subtitle => "字幕轨道(T)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_round_trip_to_actions() {
        assert_eq!(Action::decode(OPEN), Some(Action::Open));
        assert_eq!(Action::decode(SPEED_BASE + 3), Some(Action::Speed(1.)));
        assert_eq!(
            Action::decode(ASPECT_BASE + 1),
            Some(Action::Aspect("16:9"))
        );
        assert_eq!(Action::decode(ROTATE_BASE + 2), Some(Action::Rotate(180)));
        assert_eq!(
            Action::decode(AUDIO_TRACK + 2),
            Some(Action::Track(TrackKind::Audio, Some(2)))
        );
        assert_eq!(
            Action::decode(SUB_TRACK + NO_TRACK),
            Some(Action::Track(TrackKind::Subtitle, None))
        );
        assert_eq!(Action::decode(99), None);
    }

    #[test]
    fn parent_menu_titles_do_not_decode_as_actions() {
        for id in [
            SPEED_MENU,
            REPEAT_MENU,
            ASPECT_MENU,
            ROTATE_MENU,
            VIDEO_MENU,
            AUDIO_MENU,
            SUB_MENU,
        ] {
            assert!(Action::decode(id).is_none(), "{id}");
        }
    }
}
