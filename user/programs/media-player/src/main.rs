//! 媒体播放机：Windows 2000 外观的 libmpv 播放器；界面全部是共享控件，视频帧经 Canvas 媒体帧显示。
mod dialogs;
mod engine;
mod view;

use engine::{Changes, Engine};
use liteos_ui::{
    Application, ControlFlow, Dialogs, EventKind, FileDialogMode, FileFilter, MessageButtons, Task,
    WindowOptions,
};
use std::{
    cell::RefCell,
    io::Read,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::PathBuf,
    process::ExitCode,
    rc::Rc,
    time::{Duration, Instant},
};
use view::{Action, View};

const APP_ID: &str = "org.liteos.media-player";
/// 文件对话框的“媒体文件”筛选：IINA/mpv 常见的视频与音频容器；用户仍可切换“所有文件”。
const MEDIA: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts", "3gp",
    "ogv", "rmvb", "mp3", "flac", "wav", "ogg", "oga", "opus", "m4a", "aac", "wma", "ape", "ac3",
    "dts", "mka", "m3u", "m3u8", "pls",
];
const SUBTITLES: &[&str] = &["srt", "ass", "ssa", "sub", "vtt", "idx", "sup"];
/// 两次按下视频区间隔不超过它就是双击（切换全屏）。
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// 窗口、控件与引擎的共同 owner；任务句柄必须保留到完成，Drop 会取消任务。
struct Shell {
    window: liteos_ui::Window,
    view: View,
    engine: RefCell<Engine>,
    tasks: RefCell<Vec<Task<()>>>,
    // 单击延迟到双击判定完成；否则双击全屏的第一击已经切换了暂停。句柄负责取消未决单击。
    video_click: RefCell<Option<(Instant, Task<()>)>>,
}

impl Shell {
    fn sync(&self, changes: Changes) {
        let mut engine = self.engine.borrow_mut();
        if let Err(error) = self.view.apply(&mut engine, changes) {
            eprintln!("mplay: {error}");
        }
    }

    fn video_click(self: &Rc<Self>, app: &mut Application) -> liteos_ui::Result<Option<Action>> {
        let previous = self.video_click.borrow_mut().take();
        if let Some((at, task)) = previous {
            let finished = task.is_finished();
            drop(task);
            if !finished {
                if at.elapsed() <= DOUBLE_CLICK {
                    return Ok(Some(Action::Fullscreen));
                }
                // 事件积压可能让单击期限已过、定时任务尚未被 poll；先兑现上一单击。
                self.engine.borrow_mut().toggle_pause();
                self.sync(Changes {
                    controls: true,
                    ..Default::default()
                });
            }
        }
        let delay = app.sleep(DOUBLE_CLICK)?;
        let target = Rc::downgrade(self);
        let task = app.spawn_local(&self.window, async move {
            delay.await;
            if let Some(target) = target.upgrade() {
                target.engine.borrow_mut().toggle_pause();
                target.sync(Changes {
                    controls: true,
                    ..Default::default()
                });
            }
        })?;
        *self.video_click.borrow_mut() = Some((Instant::now(), task));
        Ok(None)
    }

    fn keep(&self, task: Task<()>) {
        let mut tasks = self.tasks.borrow_mut();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    fn home(&self) -> PathBuf {
        std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
    }
}

/// 读空唤醒 socket；之后由 `Engine::pump` 取走全部事件。
fn drain(reader: &UnixStream) {
    let mut buffer = [0u8; 64];
    let mut reader = reader;
    while matches!(reader.read(&mut buffer), Ok(count) if count > 0) {}
}

fn choose(
    app: &mut Application,
    shell: &Rc<Shell>,
    label: &'static str,
    extensions: &'static [&'static str],
    apply: impl FnOnce(&mut Engine, &str) + 'static,
) -> liteos_ui::Result<()> {
    let chooser = app.file_dialog_filtered(
        &shell.window,
        FileDialogMode::Open,
        &shell.home(),
        "",
        FileFilter { label, extensions },
    )?;
    let target = shell.clone();
    let task = app.spawn_local(&shell.window, async move {
        if let Ok(Some(path)) = chooser.await {
            match path.to_str() {
                Some(path) => apply(&mut target.engine.borrow_mut(), path),
                None => {
                    target.engine.borrow_mut().state.message =
                        "路径不是有效的 UTF-8，无法打开".into()
                }
            }
            target.sync(Changes {
                controls: true,
                ..Default::default()
            });
        }
    })?;
    shell.keep(task);
    Ok(())
}

fn info_rows(engine: &Engine) -> Vec<(String, String)> {
    let size = |value: String| {
        value.parse::<f64>().map_or_else(
            |_| value.clone(),
            |bytes| format!("{:.1} MiB", bytes / 1_048_576.),
        )
    };
    let pair = |engine: &Engine, name: &str| engine.text(name).filter(|text| !text.is_empty());
    let mut rows = Vec::new();
    let mut add = |label: &str, value: Option<String>| {
        if let Some(value) = value {
            rows.push((label.to_owned(), value));
        }
    };
    add("文件", pair(engine, "path"));
    add("格式", pair(engine, "file-format"));
    add("大小", pair(engine, "file-size").map(size));
    add("时长", Some(engine::clock(engine.state.duration)));
    add("标题", pair(engine, "metadata/by-key/title"));
    add("艺术家", pair(engine, "metadata/by-key/artist"));
    add("专辑", pair(engine, "metadata/by-key/album"));
    add("视频编码", pair(engine, "video-codec"));
    add(
        "分辨率",
        pair(engine, "video-params/w")
            .zip(pair(engine, "video-params/h"))
            .map(|(width, height)| format!("{width}×{height}")),
    );
    add(
        "帧率",
        pair(engine, "container-fps").map(|fps| format!("{fps} fps")),
    );
    add("音频编码", pair(engine, "audio-codec"));
    add("输出设备", pair(engine, "audio-device"));
    add("音源格式", pair(engine, "audio-params/format"));
    add("输出格式", pair(engine, "audio-out-params/format"));
    add(
        "输出采样率",
        pair(engine, "audio-out-params/samplerate").map(|rate| format!("{rate} Hz")),
    );
    add(
        "采样率",
        pair(engine, "audio-params/samplerate").map(|rate| format!("{rate} Hz")),
    );
    add(
        "声道",
        pair(engine, "audio-params/channel-count").map(|count| count.to_string()),
    );
    rows
}

fn perform(
    app: &mut Application,
    shell: &Rc<Shell>,
    action: Action,
) -> liteos_ui::Result<ControlFlow> {
    // 菜单/快捷键等显式动作取消尚未判定的画布单击，不能在动作完成后再突然暂停。
    shell.video_click.borrow_mut().take();
    let dialogs: Dialogs = app.dialogs();
    let mut refresh = Changes {
        controls: true,
        ..Default::default()
    };
    match action {
        Action::Exit => return Ok(ControlFlow::Exit),
        Action::Open => choose(app, shell, "媒体文件", MEDIA, |engine, path| {
            engine.open(path, false);
        })?,
        Action::AddToPlaylist => choose(app, shell, "媒体文件", MEDIA, |engine, path| {
            engine.open(path, true);
        })?,
        Action::AddSubtitle => choose(app, shell, "字幕文件", SUBTITLES, |engine, path| {
            engine.run(&["sub-add", path, "select"]);
        })?,
        Action::OpenUrl => {
            let target = shell.clone();
            let task = app.spawn_local(&shell.window, async move {
                match dialogs::open_url(&dialogs, &target.window).await {
                    Ok(Some(url)) => {
                        target.engine.borrow_mut().open(&url, false);
                    }
                    Ok(None) => {}
                    Err(error) => target.engine.borrow_mut().state.message = error.to_string(),
                }
                target.sync(Changes {
                    controls: true,
                    ..Default::default()
                });
            })?;
            shell.keep(task);
        }
        Action::Info => {
            let rows = info_rows(&shell.engine.borrow());
            let target = shell.clone();
            let task = app.spawn_local(&shell.window, async move {
                if let Err(error) = dialogs::media_info(&dialogs, &target.window, &rows).await {
                    eprintln!("mplay: {error}");
                }
            })?;
            shell.keep(task);
        }
        Action::About => {
            let target = shell.clone();
            let task = app.spawn_local(&shell.window, async move {
                let message = "LiteOS 媒体播放机\n\n播放引擎：mpv（libmpv）与 FFmpeg，支持常见视频、音频、字幕与网络流。";
                if let Ok(box_) = dialogs.message(&target.window, "关于媒体播放机", message, MessageButtons::Ok) {
                    let _ = box_.await;
                }
            })?;
            shell.keep(task);
        }
        Action::Fullscreen => {
            let on = !shell.view.fullscreen.get();
            shell.view.set_fullscreen(on)?;
            refresh.menus = true;
        }
        Action::TogglePlaylist => {
            shell.view.show_side(!shell.view.side_visible.get())?;
            refresh.menus = true;
            refresh.frame = true;
        }
        Action::FastScaling => {
            let on = !shell.view.fast_scaling.get();
            if shell
                .engine
                .borrow_mut()
                .set("sw-fast", if on { "yes" } else { "no" })
            {
                shell.view.fast_scaling.set(on);
            }
            refresh.menus = true;
        }
        Action::Pause => shell.engine.borrow_mut().toggle_pause(),
        Action::Stop | Action::CloseFile => {
            shell.engine.borrow_mut().run(&["stop"]);
        }
        Action::Previous => {
            shell.engine.borrow_mut().run(&["playlist-prev"]);
        }
        Action::Next => {
            shell.engine.borrow_mut().run(&["playlist-next"]);
        }
        Action::Seek(seconds) => shell.engine.borrow_mut().seek_relative(seconds),
        Action::FrameBack => {
            shell.engine.borrow_mut().run(&["frame-back-step"]);
        }
        Action::FrameStep => {
            shell.engine.borrow_mut().run(&["frame-step"]);
        }
        Action::PreviousChapter => {
            shell.engine.borrow_mut().run(&["add", "chapter", "-1"]);
        }
        Action::NextChapter => {
            shell.engine.borrow_mut().run(&["add", "chapter", "1"]);
        }
        Action::Speed(speed) => shell.engine.borrow_mut().set_speed(speed),
        Action::SpeedBy(factor) => {
            let mut engine = shell.engine.borrow_mut();
            let speed = engine.state.speed * factor;
            engine.set_speed(speed);
        }
        Action::LoopAb => {
            shell.engine.borrow_mut().run(&["ab-loop"]);
        }
        Action::Repeat(repeat) => shell.engine.borrow_mut().set_repeat(repeat),
        Action::Aspect(ratio) => {
            shell
                .engine
                .borrow_mut()
                .set("video-aspect-override", ratio);
            refresh.frame = true;
        }
        Action::Rotate(degrees) => {
            shell
                .engine
                .borrow_mut()
                .set("video-rotate", &degrees.to_string());
            refresh.frame = true;
        }
        Action::Track(kind, id) => shell.engine.borrow_mut().select_track(kind, id),
        Action::VolumeStep(delta) => shell.engine.borrow_mut().adjust_volume(delta),
        Action::Mute => {
            let mut engine = shell.engine.borrow_mut();
            let value = if engine.state.muted { "no" } else { "yes" };
            engine.set("mute", value);
        }
        Action::SharedOutput => {
            shell.engine.borrow_mut().select_output(None);
            refresh.menus = true;
        }
        Action::NativeOutput(index) => {
            let mut engine = shell.engine.borrow_mut();
            if let Some(card) = engine.state.outputs.get(index).map(|output| output.card) {
                engine.select_output(Some(card));
            }
            refresh.menus = true;
        }
        Action::RefreshOutputs => {
            shell.engine.borrow_mut().refresh_outputs();
            refresh.menus = true;
        }
        Action::AudioDelay(delta) => {
            shell
                .engine
                .borrow_mut()
                .run(&["add", "audio-delay", &delta.to_string()]);
        }
        Action::SubtitleDelay(delta) => {
            shell
                .engine
                .borrow_mut()
                .run(&["add", "sub-delay", &delta.to_string()]);
        }
        Action::SubtitleScale(delta) => {
            shell
                .engine
                .borrow_mut()
                .run(&["add", "sub-scale", &delta.to_string()]);
        }
        Action::Screenshot => {
            let mut engine = shell.engine.borrow_mut();
            if engine.run(&["screenshot"]) {
                engine.state.message = "截图已保存到“图片”文件夹".into();
            }
        }
    }
    shell.sync(refresh);
    Ok(ControlFlow::Continue)
}

/// 键盘快捷键（Linux evdev 键码）；Shift 把 5 秒跳转放大到 1 分钟。
fn key_action(code: u32, modifiers: u32) -> Option<Action> {
    let shift = modifiers & 1 != 0;
    let ctrl = modifiers & 2 != 0;
    Some(match code {
        57 => Action::Pause,
        105 => Action::Seek(if shift { -60. } else { -5. }),
        106 => Action::Seek(if shift { 60. } else { 5. }),
        103 => Action::VolumeStep(5.),
        108 => Action::VolumeStep(-5.),
        50 => Action::Mute,
        33 | 87 => Action::Fullscreen,
        51 => Action::FrameBack,
        52 => Action::FrameStep,
        49 => Action::Next,
        25 => Action::Previous,
        26 => Action::SpeedBy(0.8),
        27 => Action::SpeedBy(1.25),
        43 => Action::Speed(1.),
        24 if ctrl => Action::Open,
        _ => return None,
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new("媒体播放机", APP_ID, 720, 520))?;
    let view = View::new(&window)?;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let (engine, reader) = match Engine::create(&format!("{home}/Pictures")) {
        Ok(value) => value,
        Err(error) => {
            // 缺少 libmpv（如 riscv64）或其依赖时给出原因，而不是静默退出。
            let dialogs = app.dialogs();
            let target = window.clone();
            let message = format!("无法载入播放引擎 libmpv：{error}");
            let _task = app.spawn_local(&window, async move {
                if let Ok(box_) =
                    dialogs.message(&target, "媒体播放机", &message, MessageButtons::Ok)
                {
                    let _ = box_.await;
                }
                let _ = target.close();
            })?;
            app.run(|_, _| Ok(ControlFlow::Continue))?;
            return Err(error.into());
        }
    };
    let shell = Rc::new(Shell {
        window: window.clone(),
        view,
        engine: RefCell::new(engine),
        tasks: RefCell::new(Vec::new()),
        video_click: RefCell::new(None),
    });
    let mut wake = app.watch(OwnedFd::from(reader.try_clone()?))?;
    let pump = shell.clone();
    let _pump = app.spawn_local(&window, async move {
        while wake.readable().await.is_ok() {
            drain(&reader);
            let changes = pump.engine.borrow_mut().pump();
            pump.sync(changes);
        }
    })?;
    shell.sync(Changes::ALL);
    let mut arguments = std::env::args().skip(1);
    if let Some(first) = arguments.next() {
        let mut engine = shell.engine.borrow_mut();
        engine.open(&first, false);
        for path in arguments {
            engine.open(&path, true);
        }
    }
    app.run(|app, event| {
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        let view = &shell.view;
        let action = match event.kind {
            EventKind::Close => Some(Action::Exit),
            EventKind::Configure { scale, .. } => {
                view.scale.set(scale);
                shell.sync(Changes {
                    frame: true,
                    ..Default::default()
                });
                None
            }
            EventKind::Activate { value } if event.node == view.menu.id() => Action::decode(value),
            EventKind::Activate { value } if event.node == view.list.id() => {
                shell
                    .engine
                    .borrow_mut()
                    .play_index(value.saturating_sub(1) as usize);
                None
            }
            EventKind::Activate { .. } if event.node == view.play.id() => Some(Action::Pause),
            EventKind::Activate { .. } if event.node == view.stop.id() => Some(Action::Stop),
            EventKind::Activate { .. } if event.node == view.previous.id() => {
                Some(Action::Previous)
            }
            EventKind::Activate { .. } if event.node == view.next.id() => Some(Action::Next),
            EventKind::Changed { value } if event.node == view.seek.id() => {
                shell.engine.borrow_mut().seek_permille(value);
                None
            }
            EventKind::Changed { value } if event.node == view.volume.id() => {
                shell.engine.borrow_mut().set("volume", &value.to_string());
                None
            }
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } if event.node == view.video.id() => shell.video_click(app)?,
            EventKind::Wheel { vertical, .. } if event.node == view.video.id() => {
                Some(Action::VolumeStep(if vertical < 0 { 5. } else { -5. }))
            }
            EventKind::Key {
                code: 1,
                pressed: true,
                handled: false,
                ..
            } if view.fullscreen.get() => Some(Action::Fullscreen),
            EventKind::Key {
                code,
                pressed: true,
                modifiers,
                handled: false,
                ..
            } => key_action(code, modifiers),
            _ => None,
        };
        match action {
            Some(action) => perform(app, &shell, action),
            None => Ok(ControlFlow::Continue),
        }
    })?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mplay: {error}");
            ExitCode::FAILURE
        }
    }
}
