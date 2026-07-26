//! Generic LiteUI host: one process, one QuickJS VM, one React root and one top-level surface.

mod display;
mod font;
mod host;
mod input;
mod keymap;
#[cfg(test)]
mod pointer_capture_tests;
mod renderer;
mod style;
mod terminal;
mod terminal_font;
mod tree;

use std::{
    error::Error,
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

use display_proto::{Configure, MoveBegin};
use linux_uapi::process::SessionChild;
use linux_uapi::unix::{self, PollEvents, PollFd};
use quickjs_runtime::{Engine, Role};

use crate::{
    display::{Display, Event},
    host::{Action, Host, State},
    input::{PointerCapture, apply_event},
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
    desktop: Option<DesktopPresentation>,
    /// Stable React host node currently under the pointer, so hover-in/out can
    /// be diffed across complete per-frame rebuilds of `hits`.
    hovered: Option<u64>,
    /// Cursor shape most recently requested from the compositor, so shape
    /// changes are sent only on transition rather than on every motion.
    cursor_shape: u32,
    /// True while native scrollbar chrome owns the current pointer sequence.
    ///
    /// Without this flag a track click would page the scroll port on down and
    /// then incorrectly deliver the matching up/click to app content below it.
    native_scroll_pointer: bool,
    /// Latched keyboard modifiers for the focused text field's key→char mapping.
    /// Terminal keeps its own modifiers; this only serves UI `<input>` focus.
    modifiers: keymap::Modifiers,
}

struct DesktopPresentation {
    buffer_id: u32,
    foreign: Vec<display::ForeignLayer>,
    overlays: Vec<display::Overlay>,
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
    process_actions(
        &state,
        &mut display,
        &mut renderer,
        &mut children,
        terminal.as_mut(),
    )?;
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
        let (display_ready, terminal_ready) = wait(&display, terminal.as_ref(), &state)?;
        if display_ready {
            let event = display.next_event()?;
            if matches!(event, Event::Close) {
                return Ok(());
            }
            // A desktop-issued reconfigure swaps the surface geometry: adopt it
            // natively first so the same iteration still renders the retained
            // scene into a correctly sized fresh buffer.
            if let Event::Configure(configure) = &event {
                let configure = *configure;
                display.reconfigure(configure)?;
                renderer.set_viewport(display.logical_size());
                if let Some(terminal) = terminal.as_mut() {
                    terminal.resize(configure.width, configure.height)?;
                }
                state.invalidate_scene();
            }
            apply_event(
                &state,
                &mut engine,
                &mut renderer,
                &mut interactions,
                &display,
                event,
            )?;
            engine.run_jobs()?;
        }
        if terminal_ready && let Some(terminal) = terminal.as_mut() {
            let Some(screen) = terminal.drain()? else {
                return Ok(());
            };
            dispatch(&mut engine, "terminal", screen)?;
            engine.run_jobs()?;
        }
        // 1. `setTimeout` callbacks fire after the poll wakes on their deadline;
        //    an empty expiry means the wait ended on a display/terminal event.
        for id in state.take_expired_timers() {
            let script = format!("globalThis.__liteTimer({id});");
            engine.evaluate("lite-ui-timer.js", script.as_bytes())?;
        }
        engine.run_jobs()?;
        process_actions(
            &state,
            &mut display,
            &mut renderer,
            &mut children,
            terminal.as_mut(),
        )?;
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
    if !state.scene_is_dirty() {
        if matches!(mode, Mode::Desktop) && state.composition_is_dirty() {
            let Some(presentation) = interactions.desktop.as_ref() else {
                return Ok(());
            };
            state.take_composition_dirty();
            display.commit_desktop(
                presentation.buffer_id,
                state.focused_surface(),
                &presentation.foreign,
                &presentation.overlays,
                false,
            )?;
        }
        return Ok(());
    }
    let Some(frame) = display.acquire()? else {
        return Ok(());
    };
    let Some(scene) = state.scene_if_dirty() else {
        unreachable!("dirty state disappeared on the single UI owner thread");
    };
    let (buffer_id, output) = {
        let output = renderer.render(scene.as_slice(), frame.pixels)?;
        (frame.id, output)
    };
    match mode {
        Mode::Desktop => display.commit_desktop(
            buffer_id,
            state.focused_surface(),
            &output.foreign,
            &output.overlays,
            true,
        )?,
        Mode::App(_) => display.commit_app(buffer_id)?,
    }
    interactions.hits = output.hits;
    interactions.key_listener = output.key_listener;
    // Drop a hovered key whose region vanished from the rebuilt hit list (e.g.
    // the menu closed). The JS component unmounts and resets its own hover
    // state, so no synthetic leave is needed; just keep the tracker consistent
    // so a later re-hover of a fresh region fires enter.
    if let Some(hovered) = interactions.hovered
        && !interactions.hits.iter().any(|hit| hit.node_id == hovered)
    {
        interactions.hovered = None;
    }
    if matches!(mode, Mode::Desktop) {
        interactions.desktop = Some(DesktopPresentation {
            buffer_id,
            foreign: output.foreign,
            overlays: output.overlays,
        });
    }
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
    display: &mut Display,
    renderer: &mut Renderer,
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
            Action::BeginMove {
                surface_id,
                serial,
                min_x,
                min_y,
                max_x,
                max_y,
            } => {
                let underlay_buffer_id = {
                    let scene = state
                        .scene()
                        .ok_or("move requested before the desktop scene exists")?;
                    let frame = display
                        .acquire()?
                        .ok_or("desktop move underlay buffer unavailable")?;
                    renderer.render_move_underlay(scene.as_slice(), frame.pixels, surface_id)?;
                    frame.id
                };
                display.begin_move(MoveBegin {
                    surface_id,
                    serial,
                    underlay_buffer_id,
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                })?;
            }
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
        }
    }
    Ok(())
}

fn wait(
    display: &Display,
    terminal: Option<&Terminal>,
    state: &State,
) -> Result<(bool, bool), Box<dyn Error>> {
    if display.has_pending_event() {
        return Ok((true, false));
    }
    let mut descriptors = Vec::with_capacity(2);
    descriptors.push(PollFd::new(display.as_fd(), PollEvents::READ));
    if let Some(terminal) = terminal {
        descriptors.push(PollFd::new(terminal.as_fd(), PollEvents::READ));
    }
    // Park at most until the nearest JavaScript timer deadline so `setTimeout`
    // callbacks fire on time even when no display or terminal event arrives.
    unix::poll(&mut descriptors, state.next_timer_delay())?;
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
