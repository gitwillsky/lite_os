//! 媒体播放机（`mplay`）：Windows 2000 录音机（sndrec32）的播放部分，显示波形与位置，播放、停止、定位。
extern crate liteos_runtime as _;
use canvas::{Rect, Surface};
use classic_theme::{Edge, Rgb, SystemColor as C, Theme};
use classic_ui::{
    ButtonState, Decoration, DecorationAction, Display, DropItem, DropMenu, EventKind, FrameKind,
    Ink, Painter, Popup, Prompt, title_rects,
};
use sound::{SAMPLE_RATE, Sound, Transport};
use std::{
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};
use typeface::Typeface;

const MENU_HEIGHT: i32 = 19;
const STATUS_HEIGHT: i32 = 20;
const ESCAPE: u32 = 1;
const ENTER: u32 = 28;
const SPACE: u32 = 57;
const UP: u32 = 103;
const DOWN: u32 = 108;
const SLIDER_MAX: i32 = 1000;
const GREEN: Rgb = Rgb {
    red: 0,
    green: 255,
    blue: 0,
};
const BLACK: Rgb = Rgb {
    red: 0,
    green: 0,
    blue: 0,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Open,
    Exit,
    Play,
    Stop,
    About,
}

fn menus(playing: bool) -> Vec<DropMenu<Command>> {
    vec![
        DropMenu {
            title: "文件(F)",
            items: vec![
                DropItem::new("打开(O)...", "", Command::Open),
                DropItem::new("退出(X)", "", Command::Exit).separated(),
            ],
        },
        DropMenu {
            title: "播放(P)",
            items: vec![if playing {
                DropItem::new("停止(S)", "", Command::Stop)
            } else {
                DropItem::new("播放(P)", "空格", Command::Play)
            }],
        },
        DropMenu {
            title: "帮助(H)",
            items: vec![DropItem::new("关于媒体播放机(A)", "", Command::About)],
        },
    ]
}

/// 已打开的声音及其显示用的波形。
struct Loaded {
    name: String,
    sound: Arc<Sound>,
    /// 按固定列数预先算好的峰值；画图时按面板宽度取样。
    peaks: Vec<f32>,
}

struct Player {
    loaded: Option<Loaded>,
    transport: Arc<Transport>,
    worker: Option<JoinHandle<io::Result<()>>>,
    /// 停止时的位置（帧）；播放中以 `transport` 为准。
    cursor: usize,
    message: String,
}

impl Player {
    fn open(&mut self, path: &Path) {
        self.stop();
        match Sound::load(path) {
            Ok(sound) => {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into(),
                );
                self.loaded = Some(Loaded {
                    name,
                    peaks: sound.peaks(512),
                    sound: Arc::new(sound),
                });
                self.cursor = 0;
                self.message.clear();
            }
            Err(error) => self.message = format!("无法打开 {}：{error}", path.display()),
        }
    }

    fn playing(&self) -> bool {
        self.worker.is_some()
    }

    fn frames(&self) -> usize {
        self.loaded
            .as_ref()
            .map_or(0, |loaded| loaded.sound.frames())
    }

    fn position(&self) -> usize {
        if self.playing() {
            self.transport.position().min(self.frames())
        } else {
            self.cursor
        }
    }

    fn play(&mut self) {
        let Some(loaded) = &self.loaded else { return };
        if self.playing() {
            return;
        }
        if self.cursor >= loaded.sound.frames() {
            self.cursor = 0;
        }
        self.message.clear();
        let transport = Arc::new(Transport::default());
        self.transport = Arc::clone(&transport);
        let (sound, start) = (Arc::clone(&loaded.sound), self.cursor);
        self.worker = Some(thread::spawn(move || sound.play_from(start, &transport)));
    }

    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.transport.stop();
            let position = self.transport.position();
            self.finish(worker);
            self.cursor = position.min(self.frames());
        }
    }

    fn finish(&mut self, worker: JoinHandle<io::Result<()>>) {
        match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.message = format!("无法播放：{error}"),
            Err(_) => self.message = "播放线程异常退出".into(),
        }
    }

    /// 回收已自然结束的播放；返回状态是否变化。
    fn poll(&mut self) -> bool {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(worker) = self.worker.take()
        {
            self.finish(worker);
            // 自然播完回到开头，与录音机一致。
            self.cursor = 0;
            return true;
        }
        self.playing()
    }

    /// 拖动滑块：定位到 `fraction`（0..=1000）；播放中从新位置接着播。
    fn seek(&mut self, fraction: i32) {
        let was_playing = self.playing();
        self.stop();
        self.cursor = self.frames() * fraction.clamp(0, SLIDER_MAX) as usize / SLIDER_MAX as usize;
        if was_playing {
            self.play();
        }
    }
}

fn seconds(frames: usize) -> String {
    format!("{:.2} 秒", frames as f64 / f64::from(SAMPLE_RATE))
}

fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

struct Layout {
    bar: Rect,
    wave: Rect,
    slider: Rect,
    buttons: [Rect; 3],
    status: Rect,
}

fn layout(client: Rect) -> Layout {
    let bar = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
    let wave = Rect::new(
        client.x + 8,
        client.y + MENU_HEIGHT + 30,
        client.width - 16,
        64,
    );
    let slider = Rect::new(wave.x, wave.bottom() + 6, wave.width, 30);
    let y = slider.bottom() + 8;
    let width = (client.width - 16 - 12) / 3;
    let buttons = [0, 1, 2].map(|i| Rect::new(wave.x + i * (width + 6), y, width, 23));
    let status = Rect::new(
        client.x,
        client.bottom() - STATUS_HEIGHT,
        client.width,
        STATUS_HEIGHT,
    );
    Layout {
        bar,
        wave,
        slider,
        buttons,
        status,
    }
}

/// 绘制一帧所需的全部状态。
struct View<'a> {
    player: &'a Player,
    popup: &'a Option<Popup>,
    menus: &'a [DropMenu<Command>],
    prompt: &'a Option<Prompt>,
    pointer: (i32, i32),
}

fn paint(p: &mut Painter<'_>, client: Rect, view: &View<'_>) {
    let View {
        player,
        popup,
        menus,
        prompt,
        pointer,
    } = *view;
    let open_menu = popup.as_ref().map(|open| open.menu);
    let l = layout(client);
    let ink = Ink::Color(C::ButtonText);
    let titles = title_rects(p, l.bar, menus);
    for (index, (menu, rect)) in menus.iter().zip(&titles).enumerate() {
        let hovered = inside(*rect, pointer) && open_menu.is_none();
        p.menu_bar_item(
            *rect,
            menu.title,
            hovered || open_menu == Some(index),
            open_menu == Some(index),
        );
    }
    p.text(
        client.x + 8,
        client.y + MENU_HEIGHT + 8,
        &format!("位置: {}", seconds(player.position())),
        ink,
        false,
    );
    let length = format!("长度: {}", seconds(player.frames()));
    let width = (p.text_width(&length, false) + p.scale() - 1) / p.scale();
    p.text(
        client.right() - 8 - width,
        client.y + MENU_HEIGHT + 8,
        &length,
        ink,
        false,
    );
    // 波形：黑底、绿色竖线，已播放部分之后的位置画一条白色定位线。
    let inner = p.edge(l.wave, Edge::Sunken);
    p.fill_rgb(inner, BLACK);
    if let Some(loaded) = &player.loaded {
        let columns = inner.width.max(1) as usize;
        for column in 0..columns {
            let peak = loaded.peaks[column * loaded.peaks.len() / columns];
            let half = ((peak * (inner.height as f32 / 2.0)) as i32).max(1);
            let mid = inner.y + inner.height / 2;
            p.fill_rgb(
                Rect::new(inner.x + column as i32, mid - half, 1, half * 2),
                GREEN,
            );
        }
        let marker = player.position() * columns / player.frames().max(1);
        p.fill(
            Rect::new(inner.x + marker as i32, inner.y, 1, inner.height),
            C::ButtonHilight,
        );
    }
    let fraction = (player.position() * SLIDER_MAX as usize / player.frames().max(1)) as i32;
    p.trackbar(l.slider, false, (fraction, SLIDER_MAX), 10);
    let can_play = player.loaded.is_some();
    for (rect, label, disabled) in [
        (l.buttons[0], "到开头(B)", !can_play),
        (l.buttons[1], "播放(P)", !can_play || player.playing()),
        (l.buttons[2], "停止(S)", !player.playing()),
    ] {
        p.push_button(
            rect,
            label,
            ButtonState {
                disabled,
                ..Default::default()
            },
        );
    }
    let state = if player.playing() {
        "正在播放"
    } else {
        "已停止"
    };
    let text = if player.message.is_empty() {
        player
            .loaded
            .as_ref()
            .map_or("没有打开的文件", |loaded| loaded.name.as_str())
    } else {
        player.message.as_str()
    };
    let left = Rect::new(
        l.status.x + 2,
        l.status.y + 2,
        l.status.width - 90,
        l.status.height - 3,
    );
    let right = Rect::new(left.right() + 1, l.status.y + 2, 86, l.status.height - 3);
    for (pane, label) in [(left, text), (right, state)] {
        p.edge(pane, Edge::SunkenOuter);
        let y = pane.y + (pane.height - p.line_height()) / 2;
        p.clipped(pane, |p| {
            p.text(pane.x + 4, y, label, ink, false);
        });
    }
    if let Some(popup) = popup {
        popup.paint(p, &menus[popup.menu].items);
    }
    if let Some(prompt) = prompt {
        prompt.paint(p, client);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let window = display.create_window("媒体播放机", "org.liteos.media-player", (330, 290))?;
    let mut decoration = Decoration::new(FrameKind::Fixed);
    decoration.load_icon("media-player");
    let theme = Theme::classic();
    let mut player = Player {
        loaded: None,
        transport: Arc::new(Transport::default()),
        worker: None,
        cursor: 0,
        message: String::new(),
    };
    if let Some(path) = std::env::args().nth(1) {
        player.open(Path::new(&path));
    }
    let mut title = String::new();
    let mut popup: Option<Popup> = None;
    let mut prompt: Option<Prompt> = None;
    let mut dragging = false;
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut pointer = (-1, -1);
    let mut closing = false;
    while !closing {
        let event = display.next_event_timeout(Duration::from_millis(100))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let client = decoration.client_rect(c);
        let l = layout(client);
        let mut dirty = player.poll();
        let menu_list = menus(player.playing());
        if let Some(event) = event {
            if event.window != window {
                continue;
            }
            match decoration.event(&event.kind, c) {
                Some(DecorationAction::Request(request)) => display.request(window, request)?,
                Some(DecorationAction::Close) => break,
                Some(DecorationAction::Cursor(shape)) => display.set_cursor_shape(shape)?,
                None => {}
            }
            dirty |= !matches!(event.kind, EventKind::FramePresented { .. });
            if let EventKind::Configure(c) = &event.kind
                && scale != c.scale
            {
                font = Some(Typeface::load(
                    Path::new("/usr/share/liteos/fonts"),
                    c.scale as u32,
                )?);
                scale = c.scale;
            }
            let Some(font) = font.as_ref() else { continue };
            let mut scratch = [0u32; 1];
            let probe = Painter::new(
                Surface::new(&mut scratch, 1, 1, 1).ok_or("probe surface")?,
                &theme,
                font,
                c.scale,
            );
            let titles = title_rects(&probe, l.bar, &menu_list);
            let slider_fraction = |x: i32| {
                let start = l.slider.x + 5;
                (x - start) * SLIDER_MAX / (l.slider.width - 11).max(1)
            };
            let mut command: Option<Command> = None;
            match event.kind {
                EventKind::Close => closing = true,
                EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                    pointer = (x.floor() as i32, y.floor() as i32);
                    if let Some(open) = &mut popup {
                        if let Some(index) = titles.iter().position(|r| inside(*r, pointer))
                            && index != open.menu
                        {
                            *open = Popup::open(&probe, &menu_list, index, titles[index]);
                        }
                        open.hover = open.item_at(&menu_list[open.menu].items, pointer);
                    } else if dragging {
                        player.seek(slider_fraction(pointer.0));
                    }
                }
                EventKind::PointerLeave => pointer = (-1, -1),
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } => {
                    if let Some(open_prompt) = &prompt {
                        match open_prompt.press(client, pointer) {
                            Some(true) => {
                                player.open(&PathBuf::from(open_prompt.text()));
                                prompt = None;
                            }
                            Some(false) => prompt = None,
                            None => {}
                        }
                    } else if let Some(open) = popup.take() {
                        if let Some(item) = open.item_at(&menu_list[open.menu].items, pointer) {
                            command = Some(menu_list[open.menu].items[item].command);
                        } else if inside(open.rect, pointer) {
                            popup = Some(open);
                        }
                    } else if let Some(index) = titles.iter().position(|r| inside(*r, pointer)) {
                        popup = Some(Popup::open(&probe, &menu_list, index, titles[index]));
                    } else if player.loaded.is_some() && inside(l.slider, pointer) {
                        dragging = true;
                        player.seek(slider_fraction(pointer.0));
                    } else if inside(l.buttons[0], pointer) && player.loaded.is_some() {
                        player.seek(0);
                    } else if inside(l.buttons[1], pointer) {
                        command = Some(Command::Play);
                    } else if inside(l.buttons[2], pointer) {
                        command = Some(Command::Stop);
                    }
                }
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } => dragging = false,
                EventKind::Key(key) if key.pressed => {
                    if let Some(open_prompt) = &mut prompt {
                        match open_prompt.key(key) {
                            Some(true) => {
                                player.open(&PathBuf::from(open_prompt.text()));
                                prompt = None;
                            }
                            Some(false) => prompt = None,
                            None => {}
                        }
                    } else if let Some(open) = &mut popup {
                        let items = &menu_list[open.menu].items;
                        match key.code {
                            ESCAPE => popup = None,
                            UP => open.step(items.len(), -1),
                            DOWN => open.step(items.len(), 1),
                            ENTER => {
                                if let Some(index) = open.hover {
                                    command = Some(items[index].command);
                                    popup = None;
                                }
                            }
                            _ => {}
                        }
                    } else if key.code == SPACE {
                        command = Some(if player.playing() {
                            Command::Stop
                        } else {
                            Command::Play
                        });
                    }
                }
                _ => {}
            }
            match command {
                Some(Command::Open) => {
                    prompt = Some(Prompt::new(
                        "打开",
                        "键入要播放的 WAV 文件的完整路径。",
                        "文件名(N):",
                        "",
                    ))
                }
                Some(Command::Exit) => closing = true,
                Some(Command::Play) => player.play(),
                Some(Command::Stop) => player.stop(),
                Some(Command::About) => {
                    player.message = "LiteOS 媒体播放机：播放 16 位 PCM WAV 文件。".into()
                }
                None => {}
            }
        }
        let Some(font) = font.as_ref() else { continue };
        let wanted = player
            .loaded
            .as_ref()
            .map_or("媒体播放机".to_owned(), |loaded| {
                format!("{} - 媒体播放机", loaded.name)
            });
        if wanted != title {
            display.set_title(window, &wanted)?;
            title = wanted;
        }
        if dirty && display.ready_to_draw(window) {
            let (w, h) = c.pixel_size()?;
            let mut pixels = vec![0; w as usize * h as usize];
            let surface = Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid surface")?;
            let mut p = Painter::new(surface, &theme, font, c.scale);
            p.fill(Rect::new(0, 0, c.width, c.height), C::ButtonFace);
            decoration.paint(&mut p, c, &title);
            paint(
                &mut p,
                client,
                &View {
                    player: &player,
                    popup: &popup,
                    menus: &menu_list,
                    prompt: &prompt,
                    pointer,
                },
            );
            display.present(window, &pixels)?;
        }
    }
    player.stop();
    display.destroy_window(window)?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mplay: error: {error}");
            ExitCode::FAILURE
        }
    }
}
