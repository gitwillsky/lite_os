//! Generic LiteUI host: one process, one QuickJS VM, one React root and one top-level surface.

mod display;
mod font;
mod host;
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
    time::{Duration, Instant},
};

use display_proto::{Configure, MoveBegin};
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

/// Linux evdev `BTN_RIGHT`; the compositor forwards raw button codes and the
/// right button opens context menus rather than starting a drag or click.
const BTN_RIGHT: u32 = 273;

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
}

struct DesktopPresentation {
    buffer_id: u32,
    foreign: Vec<display::ForeignLayer>,
    overlays: Vec<display::Overlay>,
}

#[derive(Clone, Copy)]
struct PointerCapture {
    /// Stable React host node that received pointer-down.
    ///
    /// Capturing callback ids instead would break after any React commit that
    /// replaces an inline handler: later motion would target a deleted id.
    node_id: u64,
}

impl PointerCapture {
    fn hit(self, hits: &[renderer::HitRegion]) -> Option<&renderer::HitRegion> {
        hits.iter().find(|hit| hit.node_id == self.node_id)
    }

    fn move_listener(self, hits: &[renderer::HitRegion]) -> Option<u64> {
        self.hit(hits).and_then(|hit| hit.pointer_move)
    }

    fn up_listener(self, hits: &[renderer::HitRegion]) -> Option<u64> {
        self.hit(hits).and_then(|hit| hit.pointer_up)
    }
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

fn apply_event(
    state: &State,
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
    display: &Display,
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
        Event::SurfaceActivated { surface_id } => (
            "desktop",
            json!({"type":"activated","surfaceId":surface_id}),
        ),
        Event::MoveComplete { surface_id, x, y } => {
            // The compositor clamps the move destination to the authorized
            // limits, so a well-behaved MoveComplete is already in bounds. A
            // stray negative would only arise from a race or a limit
            // miscalculation; clamp it to the origin instead of tearing down
            // the whole UI process (`try_from` on a negative is a hard `?`
            // failure under `panic = "abort"`). React receives the same clamped
            // value so its canonical bounds stay in sync with native.
            let x = x.max(0);
            let y = y.max(0);
            state.move_surface(surface_id, x as u32, y as u32)?;
            (
                "desktop",
                json!({"type":"moved","surfaceId":surface_id,"x":x,"y":y}),
            )
        }
        Event::ConfigureReady { .. } => {
            state.invalidate_composition();
            return Ok(());
        }
        Event::Configure(configure) => (
            "display",
            json!({"type":"configure","width":configure.width,"height":configure.height,"serial":configure.serial}),
        ),
        Event::Pointer(pointer) => {
            dispatch_pointer(state, engine, renderer, interactions, display, pointer)?;
            return Ok(());
        }
        Event::Scroll(scroll) => {
            dispatch_scroll(state, engine, renderer, interactions, scroll)?;
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
        Event::FrameDone => return Ok(()),
        Event::Close => unreachable!("close exits before event dispatch"),
    };
    dispatch(engine, channel, payload)
}

fn dispatch_pointer(
    state: &State,
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
    display: &Display,
    pointer: display_proto::InputPointer,
) -> Result<(), Box<dyn Error>> {
    match pointer.phase {
        display_proto::PointerPhase::Down if renderer.scrollbar_at(pointer.x, pointer.y) => {
            let changed = if pointer.button == BTN_RIGHT {
                false
            } else {
                renderer.scrollbar_pointer_down(pointer.x, pointer.y).1
            };
            interactions.native_scroll_pointer = true;
            if changed {
                state.invalidate_scene();
            }
            return Ok(());
        }
        display_proto::PointerPhase::Motion if interactions.native_scroll_pointer => {
            if renderer.scrollbar_pointer_move(pointer.x, pointer.y) {
                state.invalidate_scene();
            }
            return Ok(());
        }
        display_proto::PointerPhase::Up if interactions.native_scroll_pointer => {
            interactions.native_scroll_pointer = false;
            renderer.scrollbar_pointer_up();
            return Ok(());
        }
        _ => {}
    }
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
    if renderer.scrollbar_at(pointer.x, pointer.y) {
        if pointer.phase == display_proto::PointerPhase::Motion {
            if let Some(old) = interactions.hovered
                && let Some(leave) = interactions
                    .hits
                    .iter()
                    .find(|hit| hit.node_id == old)
                    .and_then(|hit| hit.pointer_leave)
            {
                dispatch_listener(engine, leave, payload)?;
            }
            interactions.hovered = None;
            if interactions.cursor_shape != 0 {
                display.set_cursor_shape(0)?;
                interactions.cursor_shape = 0;
            }
        }
        return Ok(());
    }
    match pointer.phase {
        display_proto::PointerPhase::Down => {
            if pointer.button == BTN_RIGHT {
                // Right button opens a context menu on the topmost region that
                // asked for one. It never starts a drag, so no pointer_capture.
                if let Some(listener) = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .filter_map(|hit| hit.context_menu)
                    .next()
                {
                    dispatch_listener(engine, listener, payload.clone())?;
                }
            } else if let Some(hit) = interactions
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
                    node_id: hit.node_id,
                });
            }
        }
        display_proto::PointerPhase::Up => {
            if let Some(capture) = interactions.pointer_capture.take()
                && let Some(listener) = capture.up_listener(&interactions.hits)
            {
                dispatch_listener(engine, listener, payload.clone())?;
            }
            // Click and double-click are left-button semantics only; a right-up
            // must not fire onClick/onDoubleClick (the menu already opened on
            // right-down) nor disturb the double-click timer.
            if pointer.button != BTN_RIGHT {
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
        }
        display_proto::PointerPhase::Motion => {
            if let Some(listener) = interactions
                .pointer_capture
                .and_then(|capture| capture.move_listener(&interactions.hits))
            {
                // A held-button drag routes motion to the captured target only.
                dispatch_listener(engine, listener, payload)?;
            } else {
                // Free hover (no button held): find the topmost region under the
                // pointer that participates in hover, and diff it against the
                // last hovered region to emit leave(old) then enter(new).
                let next = interactions
                    .hits
                    .iter()
                    .rev()
                    .find(|hit| {
                        inside(hit)
                            && (hit.pointer_enter.is_some()
                                || hit.pointer_leave.is_some()
                                || hit.pointer_move.is_some())
                    })
                    .map(|hit| hit.node_id);
                if next != interactions.hovered {
                    if let Some(old) = interactions.hovered
                        && let Some(leave) = interactions
                            .hits
                            .iter()
                            .find(|hit| hit.node_id == old)
                            .and_then(|hit| hit.pointer_leave)
                    {
                        dispatch_listener(engine, leave, payload.clone())?;
                    }
                    if let Some(new) = next
                        && let Some(enter) = interactions
                            .hits
                            .iter()
                            .find(|hit| hit.node_id == new)
                            .and_then(|hit| hit.pointer_enter)
                    {
                        dispatch_listener(engine, enter, payload.clone())?;
                    }
                    interactions.hovered = next;
                }
                // Deliver an ongoing move to the hovered region if it asked for one.
                if let Some(mv) = next.and_then(|key| {
                    interactions
                        .hits
                        .iter()
                        .find(|hit| hit.node_id == key)
                        .and_then(|hit| hit.pointer_move)
                }) {
                    dispatch_listener(engine, mv, payload)?;
                }
                // Resolve the topmost region under the pointer for its cursor
                // shape (independent of hover listeners) and ask the compositor
                // to change shapes only on a transition, never per motion.
                let shape = interactions
                    .hits
                    .iter()
                    .rev()
                    .find(|hit| inside(hit))
                    .map(|hit| hit.cursor)
                    .unwrap_or(0);
                if shape != interactions.cursor_shape {
                    display.set_cursor_shape(shape)?;
                    interactions.cursor_shape = shape;
                }
            }
        }
    }
    Ok(())
}

fn dispatch_scroll(
    state: &State,
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
    scroll: display_proto::InputScroll,
) -> Result<(), Box<dyn Error>> {
    let inside = |hit: &renderer::HitRegion| {
        scroll.x as f32 >= hit.x
            && scroll.y as f32 >= hit.y
            && (scroll.x as f32) < hit.x + hit.width
            && (scroll.y as f32) < hit.y + hit.height
    };
    // Deliver to the topmost region under the pointer that asked for wheel
    // events, mirroring dispatch_pointer's `inside` + `.rev()` hit resolution.
    if let Some(listener) = interactions
        .hits
        .iter()
        .rev()
        .filter(|hit| inside(hit))
        .filter_map(|hit| hit.wheel)
        .next()
    {
        dispatch_listener(
            engine,
            listener,
            json!({
                "type":"wheel",
                "x":scroll.x,
                "y":scroll.y,
                "deltaX":scroll.delta_x,
                "deltaY":scroll.delta_y,
                "deltaMode":0
            }),
        )?;
    }
    if renderer.scroll_wheel(scroll.x, scroll.y, scroll.delta_x, scroll.delta_y) {
        state.invalidate_scene();
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
