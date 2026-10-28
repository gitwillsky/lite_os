//! mpv 引擎与播放状态镜像：所有 mpv 调用都在 UI 线程，事件由唤醒 fd 驱动。
use mpv_client::{EndReason, Event, Format, Mpv, Value};
use std::{io, os::unix::net::UnixStream};

/// 创建核心前设置的选项：用 libmpv 渲染 API 嵌入窗口，不加载用户配置、脚本与内置 OSC。
const OPTIONS: &[(&str, &str)] = &[
    ("vo", "libmpv"),
    ("ao", "alsa"),
    ("idle", "yes"),
    ("keep-open", "yes"),
    ("force-window", "no"),
    ("hwdec", "no"),
    ("osc", "no"),
    ("ytdl", "no"),
    ("load-scripts", "no"),
    ("input-default-bindings", "no"),
    ("input-vo-keyboard", "no"),
    ("sub-auto", "fuzzy"),
    ("audio-display", "embedded-first"),
    ("volume-max", "130"),
    ("screenshot-format", "png"),
    ("cache", "yes"),
    ("demuxer-max-bytes", "48MiB"),
    ("demuxer-max-back-bytes", "16MiB"),
];

const TIME: u64 = 1;
const DURATION: u64 = 2;
const PAUSE: u64 = 3;
const IDLE: u64 = 4;
const VOLUME: u64 = 5;
const MUTE: u64 = 6;
const SPEED: u64 = 7;
const TITLE: u64 = 8;
const TRACKS: u64 = 9;
const CHAPTERS: u64 = 10;
const CHAPTER: u64 = 11;
const PLAYLIST: u64 = 12;
const PLAYLIST_POS: u64 = 13;
const LOOP_FILE: u64 = 14;
const LOOP_PLAYLIST: u64 = 15;
const AB_A: u64 = 16;
const AB_B: u64 = 17;
const CACHE: u64 = 18;
const AID: u64 = 19;
const VID: u64 = 20;
const SID: u64 = 21;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

pub struct Track {
    pub id: i64,
    pub kind: TrackKind,
    pub label: String,
    pub selected: bool,
}

pub struct Entry {
    pub label: String,
    pub current: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Repeat {
    #[default]
    Off,
    One,
    All,
}

/// 本次 `pump` 或命令改变了哪些需要同步到界面的部分。
#[derive(Clone, Copy, Default)]
pub struct Changes {
    pub position: bool,
    pub controls: bool,
    pub menus: bool,
    pub playlist: bool,
    pub frame: bool,
}

impl Changes {
    pub const ALL: Self = Self {
        position: true,
        controls: true,
        menus: true,
        playlist: true,
        frame: true,
    };
    fn merge(&mut self, other: Self) {
        self.position |= other.position;
        self.controls |= other.controls;
        self.menus |= other.menus;
        self.playlist |= other.playlist;
        self.frame |= other.frame;
    }
}

pub struct State {
    pub idle: bool,
    pub paused: bool,
    pub buffering: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub title: String,
    pub repeat: Repeat,
    pub loop_a: Option<f64>,
    pub loop_b: Option<f64>,
    pub tracks: Vec<Track>,
    pub chapters: Vec<f64>,
    pub chapter: i64,
    pub playlist: Vec<Entry>,
    pub message: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            idle: true,
            paused: false,
            buffering: false,
            position: 0.,
            duration: 0.,
            volume: 100.,
            muted: false,
            speed: 1.,
            title: String::new(),
            repeat: Repeat::Off,
            loop_a: None,
            loop_b: None,
            tracks: Vec::new(),
            chapters: Vec::new(),
            chapter: -1,
            playlist: Vec::new(),
            message: String::new(),
        }
    }
}

pub struct Engine {
    mpv: Mpv,
    pub state: State,
    frame: Vec<u32>,
}

impl Engine {
    /// 载入 libmpv、创建核心并订阅状态属性；返回唤醒 socket 的读端。
    ///
    /// # Errors
    /// 缺少 libmpv 或其依赖、选项被拒绝、核心/渲染上下文创建失败。
    pub fn create(screenshots: &str) -> io::Result<(Self, UnixStream)> {
        let mut options = OPTIONS.to_vec();
        options.push(("screenshot-directory", screenshots));
        let (mpv, reader) = Mpv::create(&options)?;
        for (id, name, format) in [
            (TIME, "time-pos", Format::Double),
            (DURATION, "duration", Format::Double),
            (PAUSE, "pause", Format::Flag),
            (IDLE, "idle-active", Format::Flag),
            (VOLUME, "volume", Format::Double),
            (MUTE, "mute", Format::Flag),
            (SPEED, "speed", Format::Double),
            (TITLE, "media-title", Format::Text),
            (TRACKS, "track-list/count", Format::Int),
            (CHAPTERS, "chapter-list/count", Format::Int),
            (CHAPTER, "chapter", Format::Int),
            (PLAYLIST, "playlist/count", Format::Int),
            (PLAYLIST_POS, "playlist-pos", Format::Int),
            (LOOP_FILE, "loop-file", Format::Text),
            (LOOP_PLAYLIST, "loop-playlist", Format::Text),
            (AB_A, "ab-loop-a", Format::Text),
            (AB_B, "ab-loop-b", Format::Text),
            (CACHE, "paused-for-cache", Format::Flag),
            (AID, "aid", Format::Text),
            (VID, "vid", Format::Text),
            (SID, "sid", Format::Text),
        ] {
            mpv.observe(id, name, format)?;
        }
        Ok((
            Self {
                mpv,
                state: State::default(),
                frame: Vec::new(),
            },
            reader,
        ))
    }

    /// 处理所有排队的 mpv 事件并返回需要刷新的界面部分。
    pub fn pump(&mut self) -> Changes {
        let mut changes = Changes::default();
        while let Some(event) = self.mpv.poll() {
            let step = self.apply(event);
            changes.merge(step);
        }
        changes.frame |= self.mpv.render_ready();
        changes
    }

    fn apply(&mut self, event: Event) -> Changes {
        let mut changes = Changes::default();
        match event {
            Event::Property { id, value, .. } => changes = self.property(id, value),
            Event::EndFile(EndReason::Failed(message)) => {
                self.state.message = format!("无法播放：{message}");
                changes.controls = true;
            }
            Event::FileLoaded => {
                self.state.message.clear();
                changes = Changes::ALL;
            }
            Event::VideoReconfig | Event::PlaybackRestart => changes.frame = true,
            Event::QueueOverflow => changes = self.refresh_all(),
            _ => {}
        }
        changes
    }

    fn property(&mut self, id: u64, value: Value) -> Changes {
        let mut changes = Changes::default();
        let number = |value: &Value| match value {
            Value::Double(number) => Some(*number),
            _ => None,
        };
        match id {
            TIME => {
                self.state.position = number(&value).unwrap_or(0.);
                changes.position = true;
            }
            DURATION => {
                self.state.duration = number(&value).unwrap_or(0.);
                changes.position = true;
            }
            PAUSE => {
                self.state.paused = value == Value::Flag(true);
                changes.controls = true;
            }
            IDLE => {
                self.state.idle = value != Value::Flag(false);
                if self.state.idle {
                    self.state.position = 0.;
                    self.state.duration = 0.;
                    self.state.title.clear();
                    changes.position = true;
                }
                changes.controls = true;
                changes.menus = true;
                changes.frame = true;
            }
            VOLUME => {
                self.state.volume = number(&value).unwrap_or(100.);
                changes.controls = true;
            }
            MUTE => {
                self.state.muted = value == Value::Flag(true);
                changes.controls = true;
                changes.menus = true;
            }
            SPEED => {
                self.state.speed = number(&value).unwrap_or(1.);
                changes.controls = true;
                changes.menus = true;
            }
            TITLE => {
                if let Value::Text(text) = value {
                    self.state.title = text;
                }
                changes.controls = true;
            }
            CACHE => {
                self.state.buffering = value == Value::Flag(true);
                changes.controls = true;
            }
            TRACKS | AID | VID | SID => {
                self.refresh_tracks();
                changes.menus = true;
            }
            CHAPTERS => {
                self.refresh_chapters();
                changes.menus = true;
            }
            CHAPTER => self.state.chapter = self.mpv.int("chapter").unwrap_or(-1),
            PLAYLIST | PLAYLIST_POS => {
                self.refresh_playlist();
                changes.playlist = true;
                changes.menus = true;
            }
            LOOP_FILE | LOOP_PLAYLIST => {
                self.refresh_repeat();
                changes.menus = true;
            }
            AB_A | AB_B => {
                self.state.loop_a = self.mpv.double("ab-loop-a");
                self.state.loop_b = self.mpv.double("ab-loop-b");
                changes.controls = true;
            }
            _ => {}
        }
        changes
    }

    fn refresh_all(&mut self) -> Changes {
        self.refresh_tracks();
        self.refresh_chapters();
        self.refresh_playlist();
        self.refresh_repeat();
        self.state.position = self.mpv.double("time-pos").unwrap_or(0.);
        self.state.duration = self.mpv.double("duration").unwrap_or(0.);
        self.state.paused = self.mpv.flag("pause").unwrap_or(false);
        self.state.idle = self.mpv.flag("idle-active").unwrap_or(true);
        self.state.volume = self.mpv.double("volume").unwrap_or(100.);
        self.state.muted = self.mpv.flag("mute").unwrap_or(false);
        self.state.speed = self.mpv.double("speed").unwrap_or(1.);
        self.state.title = self.mpv.text("media-title").unwrap_or_default();
        Changes::ALL
    }

    fn refresh_tracks(&mut self) {
        let count = self.mpv.int("track-list/count").unwrap_or(0);
        self.state.tracks = (0..count)
            .filter_map(|index| {
                let field = |name: &str| self.mpv.text(&format!("track-list/{index}/{name}"));
                let kind = match field("type")?.as_str() {
                    "video" => TrackKind::Video,
                    "audio" => TrackKind::Audio,
                    "sub" => TrackKind::Subtitle,
                    _ => return None,
                };
                let id = self.mpv.int(&format!("track-list/{index}/id"))?;
                let selected = self
                    .mpv
                    .flag(&format!("track-list/{index}/selected"))
                    .unwrap_or(false);
                Some(Track {
                    id,
                    kind,
                    label: track_label(id, field("title"), field("lang"), field("codec")),
                    selected,
                })
            })
            .collect();
    }

    fn refresh_chapters(&mut self) {
        let count = self.mpv.int("chapter-list/count").unwrap_or(0);
        self.state.chapters = (0..count)
            .map(|index| {
                self.mpv
                    .double(&format!("chapter-list/{index}/time"))
                    .unwrap_or(0.)
            })
            .collect();
        self.state.chapter = self.mpv.int("chapter").unwrap_or(-1);
    }

    fn refresh_playlist(&mut self) {
        let count = self.mpv.int("playlist/count").unwrap_or(0);
        self.state.playlist = (0..count)
            .map(|index| {
                let name = self
                    .mpv
                    .text(&format!("playlist/{index}/title"))
                    .or_else(|| self.mpv.text(&format!("playlist/{index}/filename")))
                    .unwrap_or_default();
                Entry {
                    label: base_name(&name).to_owned(),
                    current: self
                        .mpv
                        .flag(&format!("playlist/{index}/current"))
                        .unwrap_or(false),
                }
            })
            .collect();
    }

    fn refresh_repeat(&mut self) {
        let on = |name: &str| {
            self.mpv
                .text(name)
                .is_some_and(|text| text != "no" && !text.is_empty())
        };
        self.state.repeat = if on("loop-file") {
            Repeat::One
        } else if on("loop-playlist") {
            Repeat::All
        } else {
            Repeat::Off
        };
    }

    /// 把当前帧渲染到 `width`×`height` 设备像素的缓冲并返回；缓冲在多次调用间复用。
    pub fn render(&mut self, width: u32, height: u32) -> io::Result<&[u32]> {
        self.frame.resize(width as usize * height as usize, 0);
        self.mpv.render(width, height, &mut self.frame)?;
        Ok(&self.frame)
    }

    /// 运行 mpv 命令；失败时把原因放进状态栏消息并返回 false。
    pub fn run(&mut self, arguments: &[&str]) -> bool {
        match self.mpv.command(arguments) {
            Ok(()) => true,
            Err(error) => {
                self.state.message =
                    format!("{}：{error}", arguments.first().copied().unwrap_or(""));
                false
            }
        }
    }

    /// 设置属性；失败时把原因放进状态栏消息。
    pub fn set(&mut self, name: &str, value: &str) -> bool {
        match self.mpv.set_property(name, value) {
            Ok(()) => true,
            Err(error) => {
                self.state.message = format!("{name}：{error}");
                false
            }
        }
    }

    /// 读取属性文本，供媒体信息对话框使用。
    pub fn text(&self, name: &str) -> Option<String> {
        self.mpv.text(name)
    }

    /// 在播放列表里加载文件；`append` 为真时加入队尾，否则替换并立即播放。
    pub fn open(&mut self, path: &str, append: bool) -> bool {
        self.run(&[
            "loadfile",
            path,
            if append { "append-play" } else { "replace" },
        ])
    }

    pub fn toggle_pause(&mut self) {
        if self.state.idle {
            return;
        }
        let value = if self.state.paused { "no" } else { "yes" };
        self.set("pause", value);
    }

    pub fn seek_relative(&mut self, seconds: f64) {
        self.run(&["seek", &seconds.to_string(), "relative"]);
    }

    /// 按 0..=1000 的比例定位（进度滑块）。
    pub fn seek_permille(&mut self, permille: i64) {
        let percent = (permille.clamp(0, 1000) as f64) / 10.;
        self.run(&["seek", &percent.to_string(), "absolute-percent"]);
    }

    pub fn adjust_volume(&mut self, delta: f64) {
        let volume = (self.state.volume + delta).clamp(0., 130.);
        self.set("volume", &volume.to_string());
    }

    /// 速度限制在 0.25–4 倍，与 IINA 的预设范围一致。
    pub fn set_speed(&mut self, speed: f64) {
        self.set("speed", &speed.clamp(0.25, 4.).to_string());
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        let (file, list) = match repeat {
            Repeat::Off => ("no", "no"),
            Repeat::One => ("inf", "no"),
            Repeat::All => ("no", "inf"),
        };
        self.set("loop-file", file);
        self.set("loop-playlist", list);
    }

    pub fn select_track(&mut self, kind: TrackKind, id: Option<i64>) {
        let property = match kind {
            TrackKind::Video => "vid",
            TrackKind::Audio => "aid",
            TrackKind::Subtitle => "sid",
        };
        let value = id.map_or_else(|| "no".to_owned(), |id| id.to_string());
        self.set(property, &value);
    }

    pub fn play_index(&mut self, index: usize) {
        self.run(&["playlist-play-index", &index.to_string()]);
    }
}

/// `1: 标题 (语言) [编码]`；缺失的部分省略。
fn track_label(
    id: i64,
    title: Option<String>,
    language: Option<String>,
    codec: Option<String>,
) -> String {
    let mut label = format!("{id}:");
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        label.push(' ');
        label.push_str(&title);
    }
    if let Some(language) = language.filter(|language| !language.is_empty()) {
        label.push_str(&format!(" ({language})"));
    }
    if let Some(codec) = codec.filter(|codec| !codec.is_empty()) {
        label.push_str(&format!(" [{codec}]"));
    }
    label
}

/// 播放列表项只显示最后一段路径。
fn base_name(name: &str) -> &str {
    name.rsplit('/')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(name)
}

/// `1:02:03` 或 `02:03`；负数与非有限值显示为 `00:00`。
pub fn clock(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0. {
        seconds as u64
    } else {
        0
    };
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats_hours_only_when_needed() {
        assert_eq!(clock(0.), "00:00");
        assert_eq!(clock(59.9), "00:59");
        assert_eq!(clock(3723.), "1:02:03");
        assert_eq!(clock(-5.), "00:00");
        assert_eq!(clock(f64::NAN), "00:00");
    }

    #[test]
    fn track_labels_skip_missing_fields() {
        assert_eq!(track_label(2, None, None, None), "2:");
        assert_eq!(
            track_label(
                1,
                Some("普通话".into()),
                Some("zh".into()),
                Some("aac".into())
            ),
            "1: 普通话 (zh) [aac]"
        );
    }

    #[test]
    fn playlist_entries_show_the_last_path_segment() {
        assert_eq!(base_name("/root/Videos/a.mkv"), "a.mkv");
        assert_eq!(base_name("http://host/video.mp4"), "video.mp4");
        assert_eq!(base_name("plain"), "plain");
    }
}
