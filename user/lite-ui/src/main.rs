//! Generic LiteUI host: one process, one QuickJS VM, one React root and one top-level surface.

mod display;
mod font;
mod host;
mod renderer;
mod style;
mod terminal;
mod tree;

use std::{
    error::Error,
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use display_proto::Configure;
use linux_uapi::process::SessionChild;
use linux_uapi::unix::{self, PollEvents, PollFd};
use quickjs_runtime::{Engine, Role};
use serde_json::json;

use crate::{
    display::{Display, Event},
    host::{Action, Host, State},
    renderer::Renderer,
    terminal::Terminal,
};

enum Mode {
    Desktop,
    App(String),
}

#[derive(Default)]
struct Interactions {
    hits: Vec<renderer::HitRegion>,
    key_listener: Option<u64>,
    pointer_capture: Option<PointerCapture>,
    last_click: Option<(Instant, i32, i32)>,
}

#[derive(Clone, Copy)]
struct PointerCapture {
    move_listener: Option<u64>,
    up_listener: Option<u64>,
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("lite-ui: invariant failure: {info}")
    }));
    if let Err(error) = run() {
        eprintln!("lite-ui: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mode = parse_mode()?;
    let (role, root) = match &mode {
        Mode::Desktop => (Role::Desktop, PathBuf::from("/usr/share/liteos/desktop")),
        Mode::App(id) => (Role::App, PathBuf::from("/usr/share/liteos/apps").join(id)),
    };
    let runtime = fs::read("/usr/lib/lite-ui/runtime.js")?;
    let source = fs::read(root.join("main.js"))?;
    let style = fs::read_to_string(root.join("style.css"))?;
    let mut display = Display::open(&mode)?;
    let mut renderer = Renderer::open(root, &style, display.logical_size())?;
    let (host, state) = Host::new(role);
    let mut engine = Engine::open(role)?;
    engine.install_host(host);
    engine.evaluate("lite-ui-runtime.js", &runtime)?;
    engine.run_jobs()?;
    engine.evaluate("main.js", &source)?;
    engine.run_jobs()?;

    let mut children = Vec::new();
    let mut terminal = match &mode {
        Mode::App(id) if id == "terminal" => Some(Terminal::spawn()?),
        _ => None,
    };
    if let Some(terminal) = terminal.as_mut() {
        eprintln!("lite-ui: terminal session ready");
        let size = display.logical_size();
        terminal.resize(size.width, size.height)?;
    }
    let mut interactions = Interactions::default();
    process_actions(&state, &display, &mut children, terminal.as_mut())?;
    render_latest(
        &mode,
        &state,
        &mut display,
        &mut renderer,
        &mut interactions,
    )?;
    match &mode {
        Mode::Desktop => eprintln!("lite-ui: desktop ready"),
        Mode::App(id) => eprintln!("lite-ui: app {id} ready"),
    }

    loop {
        let (display_ready, terminal_ready) = wait(&display, terminal.as_ref())?;
        if display_ready {
            let event = display.next_event()?;
            if matches!(event, Event::Close) {
                return Ok(());
            }
            apply_event(&state, &mut engine, &mut interactions, event)?;
            engine.run_jobs()?;
        }
        if terminal_ready && let Some(terminal) = terminal.as_mut() {
            let Some(screen) = terminal.drain()? else {
                return Ok(());
            };
            dispatch(&mut engine, "terminal", screen)?;
            engine.run_jobs()?;
        }
        process_actions(&state, &display, &mut children, terminal.as_mut())?;
        render_latest(
            &mode,
            &state,
            &mut display,
            &mut renderer,
            &mut interactions,
        )?;
        reap_children(&mut children)?;
    }
}

fn render_latest(
    mode: &Mode,
    state: &State,
    display: &mut Display,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
) -> Result<(), Box<dyn Error>> {
    let Some(scene) = state.take_scene() else {
        return Ok(());
    };
    let (buffer_id, output) = {
        let frame = display.acquire()?;
        let output = renderer.render(&scene, frame.pixels)?;
        (frame.id, output)
    };
    match mode {
        Mode::Desktop => {
            display.commit_desktop(buffer_id, state.focused_surface(), &output.foreign)?
        }
        Mode::App(_) => display.commit_app(buffer_id)?,
    }
    interactions.hits = output.hits;
    interactions.key_listener = output.key_listener;
    Ok(())
}

fn apply_event(
    state: &State,
    engine: &mut Engine,
    interactions: &mut Interactions,
    event: Event,
) -> Result<(), Box<dyn Error>> {
    let (channel, payload) = match event {
        Event::AppOpened { surface_id, app_id } => {
            state.open_surface(surface_id, app_id.clone());
            (
                "desktop",
                json!({"type":"opened","surface":{"id":surface_id,"appId":app_id}}),
            )
        }
        Event::AppClosed { surface_id } => {
            state.close_surface(surface_id);
            ("desktop", json!({"type":"closed","surfaceId":surface_id}))
        }
        Event::ConfigureReady { surface_id, serial } => (
            "desktop",
            json!({"type":"ready","surfaceId":surface_id,"serial":serial}),
        ),
        Event::Configure(configure) => (
            "display",
            json!({"type":"configure","width":configure.width,"height":configure.height,"serial":configure.serial}),
        ),
        Event::Pointer(pointer) => {
            dispatch_pointer(engine, interactions, pointer)?;
            return Ok(());
        }
        Event::Key(key) => {
            if let Some(listener) = interactions.key_listener {
                dispatch_listener(
                    engine,
                    listener,
                    json!({"type":"key","code":key.code,"value":key.value,"modifiers":key.modifiers}),
                )?;
            }
            return Ok(());
        }
        Event::Close => unreachable!("close exits before event dispatch"),
    };
    dispatch(engine, channel, payload)
}

fn dispatch_pointer(
    engine: &mut Engine,
    interactions: &mut Interactions,
    pointer: display_proto::InputPointer,
) -> Result<(), Box<dyn Error>> {
    let inside = |hit: &renderer::HitRegion| {
        pointer.x as f32 >= hit.x
            && pointer.y as f32 >= hit.y
            && (pointer.x as f32) < hit.x + hit.width
            && (pointer.y as f32) < hit.y + hit.height
    };
    let payload = json!({
        "type":"pointer",
        "phase": match pointer.phase {
            display_proto::PointerPhase::Motion => "motion",
            display_proto::PointerPhase::Down => "down",
            display_proto::PointerPhase::Up => "up",
        },
        "x":pointer.x,
        "y":pointer.y,
        "button":pointer.button,
        "buttons":pointer.buttons,
        "serial":pointer.serial
    });
    match pointer.phase {
        display_proto::PointerPhase::Down => {
            if let Some(hit) = interactions
                .hits
                .iter()
                .rev()
                .filter(|hit| inside(hit))
                .find(|hit| hit.pointer_down.is_some())
            {
                dispatch_listener(
                    engine,
                    hit.pointer_down.expect("filtered pointer listener"),
                    payload.clone(),
                )?;
                interactions.pointer_capture = Some(PointerCapture {
                    move_listener: hit.pointer_move,
                    up_listener: hit.pointer_up,
                });
            }
        }
        display_proto::PointerPhase::Up => {
            if let Some(capture) = interactions.pointer_capture.take()
                && let Some(listener) = capture.up_listener
            {
                dispatch_listener(engine, listener, payload.clone())?;
            }
            if let Some(listener) = interactions
                .hits
                .iter()
                .rev()
                .filter(|hit| inside(hit))
                .filter_map(|hit| hit.click)
                .next()
            {
                dispatch_listener(engine, listener, payload.clone())?;
            }
            let now = Instant::now();
            let double = interactions.last_click.is_some_and(|(at, x, y)| {
                now.duration_since(at) <= Duration::from_millis(500)
                    && (x - pointer.x).abs() <= 4
                    && (y - pointer.y).abs() <= 4
            });
            if double {
                if let Some(listener) = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .filter_map(|hit| hit.double_click)
                    .next()
                {
                    dispatch_listener(engine, listener, payload.clone())?;
                }
                interactions.last_click = None;
            } else {
                interactions.last_click = Some((now, pointer.x, pointer.y));
            }
        }
        display_proto::PointerPhase::Motion => {
            if let Some(listener) = interactions
                .pointer_capture
                .and_then(|capture| capture.move_listener)
            {
                dispatch_listener(engine, listener, payload)?;
            }
        }
    }
    Ok(())
}

fn dispatch_listener(
    engine: &mut Engine,
    listener: u64,
    payload: serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let payload = serde_json::to_string(&payload)?;
    let script = format!("globalThis.__liteDispatch({listener},{payload});");
    engine.evaluate("lite-ui-listener.js", script.as_bytes())?;
    Ok(())
}

fn dispatch(
    engine: &mut Engine,
    channel: &str,
    payload: serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let channel = serde_json::to_string(channel)?;
    let payload = serde_json::to_string(&payload)?;
    let script = format!("globalThis.__liteEvent({channel},{payload});");
    engine.evaluate("lite-ui-event.js", script.as_bytes())?;
    Ok(())
}

fn process_actions(
    state: &State,
    display: &Display,
    children: &mut Vec<SessionChild>,
    terminal: Option<&mut Terminal>,
) -> Result<(), Box<dyn Error>> {
    let mut terminal = terminal;
    for action in state.take_actions() {
        match action {
            Action::Launch(id) => {
                let mut command = Command::new("/bin/lite-ui");
                command.args(["--app", &id]);
                command.stdin(Stdio::null()).stdout(Stdio::null());
                children.push(SessionChild::spawn(&mut command)?);
            }
            Action::Configure {
                surface_id,
                serial,
                width,
                height,
            } => display.configure(Configure {
                surface_id,
                serial,
                width,
                height,
            })?,
            Action::Close(surface_id) => display.close(surface_id)?,
            Action::Shutdown => {
                Command::new("/sbin/poweroff")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .spawn()?;
            }
            Action::TerminalInput(payload) => terminal
                .as_deref_mut()
                .ok_or("terminal action outside terminal app")?
                .input(&payload)?,
            Action::TerminalResize { width, height } => terminal
                .as_deref_mut()
                .ok_or("terminal resize outside terminal app")?
                .resize(width, height)?,
        }
    }
    Ok(())
}

fn wait(display: &Display, terminal: Option<&Terminal>) -> Result<(bool, bool), Box<dyn Error>> {
    if display.has_pending_event() {
        return Ok((true, false));
    }
    let mut descriptors = Vec::with_capacity(2);
    descriptors.push(PollFd::new(display.as_fd(), PollEvents::READ));
    if let Some(terminal) = terminal {
        descriptors.push(PollFd::new(terminal.as_fd(), PollEvents::READ));
    }
    unix::poll(&mut descriptors, None)?;
    Ok((
        descriptors[0].returned() != PollEvents::EMPTY,
        descriptors
            .get(1)
            .is_some_and(|descriptor| descriptor.returned() != PollEvents::EMPTY),
    ))
}

fn reap_children(children: &mut Vec<SessionChild>) -> Result<(), Box<dyn Error>> {
    let mut index = 0;
    while index < children.len() {
        if children[index].try_wait()?.is_some() {
            children.swap_remove(index);
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn parse_mode() -> Result<Mode, Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    match (
        arguments.next().as_deref(),
        arguments.next(),
        arguments.next(),
    ) {
        (Some("--desktop"), None, None) => Ok(Mode::Desktop),
        (Some("--app"), Some(id), None)
            if !id.is_empty()
                && id.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                }) =>
        {
            Ok(Mode::App(id))
        }
        _ => Err("usage: lite-ui --desktop | --app <id>".into()),
    }
}
