//! 媒体播放机：共享菜单、按钮、滑块、文件对话框与异步任务；仅波形属于应用自绘。
mod playback;
use liteos_ui::{
    Application, ControlFlow, Draw, EventKind, FileDialogMode, Layout, Menu, MenuItem, Range, Rect,
    Task, Widget, WidgetKind as Kind, WidgetSpec, Window, WindowOptions, state,
};
use playback::{Loaded, Player};
use sound::SAMPLE_RATE;
use std::{cell::RefCell, path::PathBuf, process::ExitCode, rc::Rc};
const APP_ID: &str = "org.liteos.media-player";
struct View {
    window: Window,
    menu: Widget,
    wave: Widget,
    position: Widget,
    slider: Widget,
    play: Widget,
    stop: Widget,
    rewind: Widget,
    status: Widget,
}
fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}
impl View {
    fn new(window: &Window) -> liteos_ui::Result<Self> {
        let mut spec = WidgetSpec::new(Kind::Column, "");
        spec.layout.grow = 1;
        let root = window.widget(spec)?;
        let menu = child(&root, Kind::MenuBar, "", Layout::default())?;
        let body = child(
            &root,
            Kind::Column,
            "",
            Layout {
                grow: 1,
                padding: 8,
                gap: 8,
                ..Default::default()
            },
        )?;
        let wave = child(
            &body,
            Kind::Canvas,
            "",
            Layout {
                grow: 1,
                min_height: 60,
                ..Default::default()
            },
        )?;
        let position = child(
            &body,
            Kind::Label,
            "",
            Layout {
                height: 24,
                ..Default::default()
            },
        )?;
        let slider = child(
            &body,
            Kind::Slider,
            "",
            Layout {
                height: 25,
                ..Default::default()
            },
        )?;
        let actions = child(
            &body,
            Kind::Row,
            "",
            Layout {
                height: 27,
                gap: 8,
                ..Default::default()
            },
        )?;
        let rewind = child(
            &actions,
            Kind::Button,
            "回到开头",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let play = child(
            &actions,
            Kind::Button,
            "播放(P)",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let stop = child(
            &actions,
            Kind::Button,
            "停止(S)",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let status = child(&root, Kind::Status, "", Layout::default())?;
        Ok(Self {
            window: window.clone(),
            menu,
            wave,
            position,
            slider,
            play,
            stop,
            rewind,
            status,
        })
    }
    fn update(&self, player: &Player) -> liteos_ui::Result<()> {
        let available = player.loaded.is_some() && !player.loading && !player.closing;
        let title = player
            .loaded
            .as_ref()
            .map_or("媒体播放机".into(), |loaded| {
                format!("{} - 媒体播放机", loaded.name)
            });
        self.window.set_title(&title)?;
        self.position.set_text(&format!(
            "位置：{}\t长度：{}",
            seconds(player.position()),
            seconds(player.frames())
        ))?;
        self.slider
            .set_state(if available { 0 } else { state::DISABLED })?;
        self.slider.set_range(Range {
            minimum: 0,
            maximum: 1000,
            value: ((player.position() as u128 * 1000) / (player.frames().max(1) as u128)) as i32,
            page: 1,
            step: 10,
            vertical: 0,
        })?;
        self.play.set_state(if available && !player.playing() {
            0
        } else {
            state::DISABLED
        })?;
        self.stop
            .set_state(if player.playing() { 0 } else { state::DISABLED })?;
        self.rewind
            .set_state(if available { 0 } else { state::DISABLED })?;
        let open = [
            MenuItem {
                id: 1,
                label: "打开(O)...",
                shortcut: "",
                state: if player.loading { 1 } else { 0 },
                parent: 0,
            },
            MenuItem {
                id: 2,
                label: "退出(X)",
                shortcut: "",
                state: 4,
                parent: 0,
            },
        ];
        let action = [MenuItem {
            id: if player.playing() { 4 } else { 3 },
            label: if player.playing() {
                "停止(S)"
            } else {
                "播放(P)"
            },
            shortcut: "空格",
            state: if available { 0 } else { 1 },
            parent: 0,
        }];
        let help = [MenuItem {
            id: 5,
            label: "关于媒体播放机(A)",
            shortcut: "",
            state: 0,
            parent: 0,
        }];
        self.menu.set_menus(&[
            Menu {
                title: "文件(F)",
                items: &open,
            },
            Menu {
                title: "播放(P)",
                items: &action,
            },
            Menu {
                title: "帮助(H)",
                items: &help,
            },
        ])?;
        self.status.set_text(&player.message)?;
        self.waveform(player)
    }
    fn waveform(&self, player: &Player) -> liteos_ui::Result<()> {
        let area = self.wave.geometry()?;
        if area.width <= 0 || area.height <= 0 {
            return Ok(());
        }
        let mut commands = vec![Draw {
            kind: 1,
            rect: Rect::new(0, 0, area.width, area.height),
            color: 0,
            bold: false,
            text: "",
        }];
        let center = area.height / 2;
        commands.push(Draw {
            kind: 1,
            rect: Rect::new(0, center, area.width, 1),
            color: 0x008000,
            bold: false,
            text: "",
        });
        if let Some(loaded) = &player.loaded {
            if !loaded.peaks.is_empty() {
                for x in 0..area.width {
                    let index = x as usize * loaded.peaks.len() / area.width as usize;
                    let peak =
                        (loaded.peaks[index].clamp(0., 1.) * (center - 2).max(0) as f32) as i32;
                    commands.push(Draw {
                        kind: 1,
                        rect: Rect::new(x, center - peak, 1, peak * 2 + 1),
                        color: 0x00ff00,
                        bold: false,
                        text: "",
                    });
                }
            }
            let x = (player.position() as u128 * (area.width - 1) as u128
                / player.frames().max(1) as u128) as i32;
            commands.push(Draw {
                kind: 1,
                rect: Rect::new(x, 0, 1, area.height),
                color: 0xffffff,
                bold: false,
                text: "",
            });
        }
        self.wave.set_drawing(&commands)
    }
}
fn seconds(frames: usize) -> String {
    format!("{:.2} 秒", frames as f64 / f64::from(SAMPLE_RATE))
}
fn directory(player: &Player) -> PathBuf {
    player
        .loaded
        .as_ref()
        .and_then(|loaded| loaded.path.parent().map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"))
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new("媒体播放机", APP_ID, 370, 310))?;
    let view = Rc::new(View::new(&window)?);
    let player = Rc::new(RefCell::new(Player::new()));
    let background = app.background()?;
    let clock = app.clock();
    let refresh_view = view.clone();
    let refresh_player = player.clone();
    let _refresh = app.spawn_local(&window, async move {
        loop {
            let seen = refresh_player.borrow().revision();
            if let Err(error) = refresh_view.update(&refresh_player.borrow()) {
                eprintln!("mplay: {error}");
                break;
            }
            if let Err(error) = Player::wait_changed(&refresh_player, &clock, seen).await {
                eprintln!("mplay: {error}");
                break;
            }
        }
    })?;
    let mut load: Option<Task<()>> = None;
    if let Some(path) = std::env::args_os().nth(1) {
        player.borrow_mut().begin_load();
        let work = background.run(move || Loaded::load(path.into()))?;
        let target = player.clone();
        load = Some(app.spawn_local(&window, async move {
            let result = work.await;
            target.borrow_mut().loaded(result);
        })?);
    }
    let mut audio: Option<Task<()>> = None;
    let mut close: Option<Task<()>> = None;
    app.run(|app, event| {
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        if player.borrow().closing {
            return Ok(ControlFlow::PreventDefault);
        }
        let mut action = None;
        match event.kind {
            EventKind::Close => action = Some(2),
            EventKind::Configure { .. } => view.waveform(&player.borrow())?,
            EventKind::Activate { value } if event.node == view.menu.id() => action = Some(value),
            EventKind::Activate { .. } if event.node == view.play.id() => action = Some(3),
            EventKind::Activate { .. } if event.node == view.stop.id() => action = Some(4),
            EventKind::Activate { .. } if event.node == view.rewind.id() => {
                player.borrow_mut().seek(0)
            }
            EventKind::Changed { value } if event.node == view.slider.id() => {
                player.borrow_mut().seek(value)
            }
            EventKind::Key {
                code: 57,
                pressed: true,
                handled: false,
                ..
            } => action = Some(if player.borrow().playing() { 4 } else { 3 }),
            EventKind::Key {
                code: 24,
                pressed: true,
                modifiers,
                handled: false,
                ..
            } if modifiers & 2 != 0 => action = Some(1),
            _ => {}
        }
        match action {
            Some(1) if !player.borrow().loading => {
                let chooser = app.file_dialog_filtered(
                    &window,
                    FileDialogMode::Open,
                    &directory(&player.borrow()),
                    "",
                    liteos_ui::FileFilter {
                        label: "Wave 音频 (*.wav)",
                        extensions: &["wav"],
                    },
                )?;
                let player = player.clone();
                let background = background.clone();
                load = Some(app.spawn_local(&window, async move {
                    match chooser.await {
                        Ok(Some(path)) => {
                            player.borrow_mut().begin_load();
                            match background.run(move || Loaded::load(path)) {
                                Ok(work) => {
                                    let result = work.await;
                                    player.borrow_mut().loaded(result);
                                }
                                Err(error) => player
                                    .borrow_mut()
                                    .loaded(Err(std::io::Error::other(error.to_string()))),
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let mut player = player.borrow_mut();
                            player.message = error.to_string();
                            player.changed();
                        }
                    }
                })?);
            }
            Some(2) => {
                player.borrow_mut().closing = true;
                let target = player.clone();
                let window = window.clone();
                close = Some(app.spawn(async move {
                    Player::shutdown(&target).await;
                    let _ = window.close();
                })?);
                return Ok(ControlFlow::PreventDefault);
            }
            Some(3) => {
                let job = player.borrow_mut().play();
                if let Some(job) = job {
                    audio = Some(app.spawn_local(
                        &window,
                        playback::drive(player.clone(), background.clone(), job),
                    )?);
                }
            }
            Some(4) => player.borrow_mut().stop(),
            Some(5) => {
                let mut player = player.borrow_mut();
                player.message = "LiteOS 媒体播放机：播放 16 位 PCM WAV 文件。".into();
                player.changed();
            }
            _ => {}
        }
        Ok(ControlFlow::Continue)
    })?;
    drop((load, audio, close));
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
