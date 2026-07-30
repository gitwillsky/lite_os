//! Generic LiteUI host: one process, one QuickJS VM, one React root and one top-level surface.

mod audio;
mod color;
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
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

use display_proto::{Configure, MoveBegin};
use linux_uapi::process::{SessionChild, SessionCommand, SessionIo};
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
    /// Latest compositor-routed pointer position used to recompute CSS cursor
    /// after a DOM/style-only frame. Without it, removing or covering the
    /// hovered element would leave its old cursor visible until physical motion.
    pointer_position: Option<(i32, i32)>,
    /// True while native scrollbar chrome owns the current pointer sequence.
    ///
    /// Without this flag a track click would page the scroll port on down and
    /// then incorrectly deliver the matching up/click to app content below it.
    native_scroll_pointer: bool,
    /// Latched keyboard modifiers for the focused text field's key→char mapping.
    /// Terminal keeps its own modifiers; this only serves UI `<input>` focus.
    modifiers: keymap::Modifiers,
    /// One native text-field paste awaiting its exact compositor response.
    ///
    /// Without the node identity, an asynchronous reply could paste into a
    /// different field after focus changed.
    pending_clipboard_paste: Option<input::ClipboardPaste>,
    /// Counts native paste requests from the top of the u64 namespace so they
    /// cannot collide with JavaScript Clipboard API request identities.
    native_clipboard_generation: u64,
}

struct DesktopPresentation {
    buffer_id: u32,
    foreign: Vec<display::ForeignLayer>,
    windows: Vec<display::WindowFrame>,
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
    let runtime = fs::read("/usr/lib/lite-ui/runtime.js")
        .map_err(|error| owner_error("runtime bundle read", error))?;
    let source =
        fs::read(root.join("main.js")).map_err(|error| owner_error("app bundle read", error))?;
    let style = fs::read_to_string(root.join("style.css"))
        .map_err(|error| owner_error("app style read", error))?;
    let mut display = Display::open(&mode).map_err(|error| owner_error("display open", error))?;
    let mut renderer = Renderer::open(root.clone(), &style, display.logical_size())
        .map_err(|error| owner_error("renderer open", error))?;
    let audio_role = if matches!(mode, Mode::Desktop) {
        audio_proto::ClientRole::Desktop
    } else {
        audio_proto::ClientRole::Media
    };
    let (audio_commands, mut audio_events) =
        audio::start(audio_role).map_err(|error| owner_error("audio start", error))?;
    let (host, state) = Host::new(
        role,
        root.clone(),
        PathBuf::from("/usr/share/liteos/apps"),
        audio_commands,
        display.logical_size(),
    );
    let mut engine = Engine::open(role).map_err(|error| owner_error("engine open", error))?;
    engine.install_host(host);
    engine
        .evaluate("lite-ui-runtime.js", &runtime)
        .map_err(|error| owner_error("runtime evaluate", error))?;
    engine
        .run_jobs()
        .map_err(|error| owner_error("runtime jobs", error))?;
    engine
        .evaluate("main.js", &source)
        .map_err(|error| owner_error("app evaluate", error))?;
    engine
        .run_jobs()
        .map_err(|error| owner_error("app jobs", error))?;

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
    )
    .map_err(|error| owner_error("startup host actions", error))?;
    render_latest(
        &mode,
        &state,
        &mut display,
        &mut renderer,
        &mut interactions,
    )
    .map_err(|error| owner_error("startup scene render", error))?;
    let ready_marker = match &mode {
        Mode::Desktop => "lite-ui: desktop ready\n".to_owned(),
        Mode::App(id) => format!("lite-ui: app {id} ready\n"),
    };
    // One write keeps the cross-process runtime marker intact; formatted stderr
    // fragments can interleave with compositor or sibling-app diagnostics.
    io::stderr().write_all(ready_marker.as_bytes())?;
    // Tracks the startup CSS motion until its terminal frame is physically
    // presented. Without this distinction, synthetic input can arrive after
    // React mounted the desktop but while the full-screen Splash still owns
    // pointer targeting, so a resize gate presses the wrong element.
    let mut startup_motion_pending = matches!(mode, Mode::Desktop) && renderer.animations_active();
    if matches!(mode, Mode::Desktop) && !startup_motion_pending {
        io::stderr().write_all(b"lite-ui: desktop startup motion settled\n")?;
    }

    loop {
        let (display_ready, terminal_ready, audio_ready) =
            wait(&display, terminal.as_ref(), &audio_events, &state)
                .map_err(|error| owner_error("event wait", error))?;
        if display_ready {
            let event = display
                .next_event()
                .map_err(|error| owner_error("display event", error))?;
            if matches!(event, Event::Close) {
                return Ok(());
            }
            // A CSS document requests its next sample only after the previous
            // buffer really reached scanout. Accepted/released acknowledgements
            // never advance animation time, preventing timer-style overproduction
            // and making back-pressure an intrinsic part of the refresh driver.
            if let Event::Presented { monotonic_ns } = &event {
                renderer.presented(*monotonic_ns);
                if renderer.animations_active() {
                    state.invalidate_scene();
                } else if startup_motion_pending {
                    io::stderr().write_all(b"lite-ui: desktop startup motion settled\n")?;
                    startup_motion_pending = false;
                }
            }
            // A desktop-issued reconfigure swaps the surface geometry: adopt it
            // natively first so the same iteration still renders the retained
            // scene into a correctly sized fresh buffer.
            if let Event::Configure(configure) = &event {
                let configure = *configure;
                display
                    .reconfigure(configure)
                    .map_err(|error| owner_error("display reconfigure", error))?;
                renderer.set_viewport(display.logical_size());
                state.set_viewport(display.logical_size());
                dispatch_viewport(&mut engine, display.logical_size())
                    .map_err(|error| owner_error("viewport resize dispatch", error))?;
                if let Some(terminal) = terminal.as_mut() {
                    terminal
                        .resize(configure.width, configure.height)
                        .map_err(|error| owner_error("terminal resize", error))?;
                }
                state.invalidate_scene();
            }
            if let Event::OutputConfigure(configure) = &event {
                display
                    .reconfigure_output(*configure)
                    .map_err(|error| owner_error("output reconfigure", error))?;
                let viewport = display.logical_size();
                renderer.set_viewport(viewport);
                state.set_viewport(viewport);
                dispatch_viewport(&mut engine, viewport)
                    .map_err(|error| owner_error("viewport resize dispatch", error))?;
                state.invalidate_scene();
            }
            apply_event(
                &state,
                &mut engine,
                &mut renderer,
                &mut interactions,
                &display,
                event,
            )
            .map_err(|error| owner_error("input dispatch", error))?;
            engine
                .run_jobs()
                .map_err(|error| owner_error("input JavaScript jobs", error))?;
        }
        if terminal_ready && let Some(terminal) = terminal.as_mut() {
            let Some(screen) = terminal
                .drain()
                .map_err(|error| owner_error("terminal drain", error))?
            else {
                return Ok(());
            };
            dispatch(&mut engine, "terminal", screen)
                .map_err(|error| owner_error("terminal event dispatch", error))?;
            engine
                .run_jobs()
                .map_err(|error| owner_error("terminal JavaScript jobs", error))?;
        }
        if audio_ready {
            for event in audio_events
                .drain()
                .map_err(|error| owner_error("audio event drain", error))?
            {
                let channel = event.channel();
                let value = serde_json::to_value(event)
                    .map_err(|error| owner_error("audio event encode", error))?;
                dispatch(&mut engine, channel, value)
                    .map_err(|error| owner_error("audio event dispatch", error))?;
            }
            engine
                .run_jobs()
                .map_err(|error| owner_error("audio JavaScript jobs", error))?;
        }
        // 1. `setTimeout` callbacks fire after the poll wakes on their deadline;
        //    an empty expiry means the wait ended on a display/terminal event.
        for id in state.take_expired_timers() {
            let script = format!("globalThis.__liteTimer({id});");
            engine
                .evaluate("lite-ui-timer.js", script.as_bytes())
                .map_err(|error| owner_error("timer callback", error))?;
        }
        engine
            .run_jobs()
            .map_err(|error| owner_error("timer JavaScript jobs", error))?;
        process_actions(
            &state,
            &mut display,
            &mut renderer,
            &mut children,
            terminal.as_mut(),
        )
        .map_err(|error| owner_error("host actions", error))?;
        render_latest(
            &mode,
            &state,
            &mut display,
            &mut renderer,
            &mut interactions,
        )
        .map_err(|error| owner_error("scene render", error))?;
        reap_children(&mut children).map_err(|error| owner_error("child reap", error))?;
    }
}

fn owner_error(owner: &'static str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{owner}: {error}"))
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
                &presentation.windows,
                &presentation.overlays,
                &[],
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
            &output.windows,
            &output.overlays,
            &output.damage,
        )?,
        Mode::App(_) => display.commit_app(buffer_id, &output.damage)?,
    }
    interactions.hits = output.hits;
    interactions.key_listener = output.key_listener;
    input::reconcile_cursor(interactions, display)?;
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
            windows: output.windows,
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

fn dispatch_viewport(
    engine: &mut Engine,
    viewport: display_proto::Size,
) -> Result<(), Box<dyn Error>> {
    dispatch(
        engine,
        "viewport",
        serde_json::json!({
            "width": viewport.width,
            "height": viewport.height,
            "devicePixelRatio": display_proto::DEVICE_SCALE_FACTOR,
        }),
    )
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
                children.push(SessionChild::spawn(SessionCommand::new(
                    "/bin/lite-ui",
                    vec!["--app".into(), id.into()],
                    SessionIo::Background,
                ))?);
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
            Action::SetAccelerators(chords) => display.set_accelerators(&chords)?,
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
            Action::TerminalPaste(text) => terminal
                .as_deref_mut()
                .ok_or("terminal paste outside terminal app")?
                .paste(&text)?,
            Action::ClipboardRead(request_id) => display.clipboard_read(request_id)?,
            Action::ClipboardWrite(text) => display.clipboard_write(text)?,
        }
    }
    Ok(())
}

fn wait(
    display: &Display,
    terminal: Option<&Terminal>,
    audio: &audio::Events,
    state: &State,
) -> Result<(bool, bool, bool), Box<dyn Error>> {
    if display.has_pending_event() {
        return Ok((true, false, false));
    }
    let mut descriptors = Vec::with_capacity(3);
    descriptors.push(PollFd::new(display.as_fd(), PollEvents::READ));
    if let Some(terminal) = terminal {
        descriptors.push(PollFd::new(terminal.as_fd(), PollEvents::READ));
    }
    let audio_index = descriptors.len();
    descriptors.push(PollFd::new(audio.as_fd(), PollEvents::READ));
    // Park at most until the nearest JavaScript timer deadline so `setTimeout`
    // callbacks fire on time even when no display or terminal event arrives.
    unix::poll(&mut descriptors, state.next_timer_delay())?;
    Ok((
        descriptors[0].returned() != PollEvents::EMPTY,
        terminal.is_some() && descriptors[1].returned() != PollEvents::EMPTY,
        descriptors[audio_index].returned() != PollEvents::EMPTY,
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
