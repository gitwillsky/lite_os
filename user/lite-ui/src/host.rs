//! Fixed native operations exposed to the self-contained React bundle.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use quickjs_runtime::{EngineError, NativeHost, Role};
use serde::Serialize;

use crate::tree::{self, Node};

/// One side effect requested synchronously by React and executed after its JS turn.
pub enum Action {
    /// Launch one checked application registry id.
    Launch(String),
    /// Route a desktop-owned app configure.
    Configure {
        surface_id: u32,
        serial: u64,
        width: u32,
        height: u32,
    },
    /// Route an unconditional app close.
    Close(u32),
    /// Request system shutdown.
    Shutdown,
    /// Send bytes to the terminal helper.
    TerminalInput(Vec<u8>),
    /// Resize the terminal helper viewport.
    TerminalResize { width: u32, height: u32 },
}

#[derive(Clone, Serialize)]
struct Bounds {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Serialize)]
struct Surface {
    id: u32,
    app_id: String,
    title: String,
    icon: String,
    bounds: Bounds,
    #[serde(skip)]
    configure: Option<(u32, u32, u64)>,
}

/// Latest-only UI state and deferred native actions shared with the event loop.
pub struct State {
    scene: RefCell<Option<Vec<Node>>>,
    actions: RefCell<Vec<Action>>,
    surfaces: RefCell<Vec<Surface>>,
    next_configure: Cell<u64>,
    focused_surface: Cell<u32>,
    timers: RefCell<Vec<(u64, Instant)>>,
}

impl State {
    /// Takes the most recent complete React host snapshot.
    pub fn take_scene(&self) -> Option<Vec<Node>> {
        self.scene.borrow_mut().take()
    }

    /// Takes all native actions produced by the completed JavaScript turn.
    pub fn take_actions(&self) -> Vec<Action> {
        std::mem::take(&mut *self.actions.borrow_mut())
    }

    /// Takes every JavaScript timer whose deadline has passed.
    pub fn take_expired_timers(&self) -> Vec<u64> {
        let now = Instant::now();
        let mut timers = self.timers.borrow_mut();
        let expired = timers
            .iter()
            .filter(|(_, deadline)| *deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        timers.retain(|(_, deadline)| *deadline > now);
        expired
    }

    /// Returns how long the event loop may park until the next timer fires.
    pub fn next_timer_delay(&self) -> Option<Duration> {
        let now = Instant::now();
        self.timers
            .borrow()
            .iter()
            .map(|(_, deadline)| deadline.saturating_duration_since(now))
            .min()
    }

    /// Returns the desktop-selected focused app surface.
    pub fn focused_surface(&self) -> u32 {
        self.focused_surface.get()
    }

    /// Adds one compositor-published app surface to desktop policy state.
    pub fn open_surface(&self, id: u32, app_id: String) {
        let index = self.surfaces.borrow().len() as u32;
        let (title, icon) = app_metadata(&app_id);
        self.surfaces.borrow_mut().push(Surface {
            id,
            app_id,
            title: title.to_owned(),
            icon: icon.to_owned(),
            bounds: Bounds {
                x: 150 + index * 28,
                y: 90 + index * 24,
                width: 720,
                height: 480,
            },
            configure: None,
        });
        self.focused_surface.set(id);
    }

    /// Removes one disconnected app surface from desktop policy state.
    pub fn close_surface(&self, id: u32) {
        self.surfaces
            .borrow_mut()
            .retain(|surface| surface.id != id);
        if self.focused_surface.get() == id {
            self.focused_surface.set(
                self.surfaces
                    .borrow()
                    .last()
                    .map_or(0, |surface| surface.id),
            );
        }
    }

    fn move_surface(&self, id: u32, x: u32, y: u32) -> Result<(), EngineError> {
        let mut surfaces = self.surfaces.borrow_mut();
        let surface = surfaces
            .iter_mut()
            .find(|surface| surface.id == id)
            .ok_or_else(|| EngineError::from_host("move targets unknown surface"))?;
        surface.bounds.x = x;
        surface.bounds.y = y;
        Ok(())
    }
}

/// QuickJS native bridge implementation for one LiteUI process.
pub struct Host {
    role: Role,
    started: Instant,
    state: Rc<State>,
}

impl Host {
    /// Creates the unique host and its read-side state handle.
    pub fn new(role: Role) -> (Self, Rc<State>) {
        let state = Rc::new(State {
            scene: RefCell::new(None),
            actions: RefCell::new(Vec::new()),
            surfaces: RefCell::new(Vec::new()),
            next_configure: Cell::new(1),
            focused_surface: Cell::new(0),
            timers: RefCell::new(Vec::new()),
        });
        (
            Self {
                role,
                started: Instant::now(),
                state: state.clone(),
            },
            state,
        )
    }

    fn desktop_configure(&self, payload: &str) -> Result<String, EngineError> {
        let mut fields = payload.split(':');
        let surface_id = parse_u32(fields.next(), "surface id")?;
        let width = parse_u32(fields.next(), "surface width")?;
        let height = parse_u32(fields.next(), "surface height")?;
        if fields.next().is_some() || width == 0 || height == 0 {
            return Err(EngineError::from_host("invalid desktop configure"));
        }
        let mut surfaces = self.state.surfaces.borrow_mut();
        let surface = surfaces
            .iter_mut()
            .find(|surface| surface.id == surface_id)
            .ok_or_else(|| EngineError::from_host("configure targets unknown surface"))?;
        if let Some((old_width, old_height, serial)) = surface.configure
            && old_width == width
            && old_height == height
        {
            return Ok(serial.to_string());
        }
        let serial = self.state.next_configure.get();
        self.state.next_configure.set(
            serial
                .checked_add(1)
                .ok_or_else(|| EngineError::from_host("configure identity exhausted"))?,
        );
        surface.configure = Some((width, height, serial));
        self.state.actions.borrow_mut().push(Action::Configure {
            surface_id,
            serial,
            width,
            height,
        });
        Ok(serial.to_string())
    }
}

impl NativeHost for Host {
    fn invoke(&mut self, operation: &str, payload: &str) -> Result<String, EngineError> {
        match operation {
            "scene.commit" => {
                let scene = tree::parse(payload).map_err(EngineError::from_host)?;
                self.state.scene.replace(Some(scene));
                Ok(String::new())
            }
            "time.now" => Ok(self
                .started
                .elapsed()
                .as_secs_f64()
                .mul_add(1000.0, 0.0)
                .to_string()),
            // 1. Wall-clock seconds for desktop chrome (the tray clock); the
            //    monotonic `time.now` above stays for animation timing.
            "time.clock" => Ok(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| EngineError::from_host(error.to_string()))?
                .as_secs()
                .to_string()),
            "timer.set" => {
                let mut fields = payload.split(':');
                let id = parse_u64(fields.next(), "timer id")?;
                let delay = parse_u64(fields.next(), "timer delay")?;
                if fields.next().is_some() {
                    return Err(EngineError::from_host("invalid timer set"));
                }
                self.state
                    .timers
                    .borrow_mut()
                    .push((id, Instant::now() + Duration::from_millis(delay)));
                Ok(String::new())
            }
            "timer.clear" => {
                let id = parse_u64(Some(payload), "timer id")?;
                self.state
                    .timers
                    .borrow_mut()
                    .retain(|(timer, _)| *timer != id);
                Ok(String::new())
            }
            "apps.list" if self.role == Role::Desktop => Ok(
                r#"[{"id":"terminal","name":"Terminal","description":"Command line","icon":"assets/terminal.png"}]"#.to_owned(),
            ),
            "apps.launch" if self.role == Role::Desktop && valid_app_id(payload) => {
                self.state.actions.borrow_mut().push(Action::Launch(payload.to_owned()));
                Ok(String::new())
            }
            "desktop.surfaces" if self.role == Role::Desktop => serde_json::to_string(&*self.state.surfaces.borrow()).map_err(|error| EngineError::from_host(error.to_string())),
            "desktop.configure" if self.role == Role::Desktop => self.desktop_configure(payload),
            "desktop.focus" if self.role == Role::Desktop => {
                let surface_id = parse_u32(Some(payload), "focused surface")?;
                self.state.focused_surface.set(surface_id);
                Ok(String::new())
            }
            "desktop.move" if self.role == Role::Desktop => {
                let mut fields = payload.split(':');
                let surface_id = parse_u32(fields.next(), "moved surface")?;
                let x = parse_u32(fields.next(), "surface x")?;
                let y = parse_u32(fields.next(), "surface y")?;
                if fields.next().is_some() {
                    return Err(EngineError::from_host("invalid desktop move"));
                }
                self.state.move_surface(surface_id, x, y)?;
                Ok(String::new())
            }
            "desktop.close" if self.role == Role::Desktop => {
                self.state.actions.borrow_mut().push(Action::Close(parse_u32(Some(payload), "closed surface")?));
                Ok(String::new())
            }
            "desktop.shutdown" if self.role == Role::Desktop => {
                self.state.actions.borrow_mut().push(Action::Shutdown);
                Ok(String::new())
            }
            "terminal.connect" if self.role == Role::App => Ok(
                r#"{"rows":["Connecting to LiteOS terminal…"],"cursor":{"column":0,"row":0}}"#.to_owned(),
            ),
            "terminal.input" if self.role == Role::App => {
                self.state.actions.borrow_mut().push(Action::TerminalInput(payload.as_bytes().to_vec()));
                Ok(String::new())
            }
            "terminal.resize" if self.role == Role::App => {
                let mut fields = payload.split(':');
                let width = parse_u32(fields.next(), "terminal width")?;
                let height = parse_u32(fields.next(), "terminal height")?;
                if fields.next().is_some() { return Err(EngineError::from_host("invalid terminal resize")); }
                self.state.actions.borrow_mut().push(Action::TerminalResize { width, height });
                Ok(String::new())
            }
            _ => Err(EngineError::from_host(format!(
                "operation '{operation}' is unavailable in this session"
            ))),
        }
    }
}

fn app_metadata(id: &str) -> (&'static str, &'static str) {
    match id {
        "terminal" => ("Terminal", "assets/terminal.png"),
        _ => ("Application", "assets/terminal.png"),
    }
}

fn parse_u32(value: Option<&str>, name: &str) -> Result<u32, EngineError> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| EngineError::from_host(format!("invalid {name}")))
}

fn parse_u64(value: Option<&str>, name: &str) -> Result<u64, EngineError> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| EngineError::from_host(format!("invalid {name}")))
}

fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use quickjs_runtime::{Engine, Role};

    use super::Host;

    #[test]
    fn quickjs_bridge_publishes_only_the_latest_complete_scene() {
        let (host, state) = Host::new(Role::Desktop);
        let mut engine = Engine::open(Role::Desktop).expect("desktop engine must open");
        engine.install_host(host);
        engine
            .evaluate(
                "host.js",
                br##"
                __liteNative("scene.commit", '[{"type":"view","props":{},"children":[]}]');
                __liteNative("scene.commit", '[{"type":"text","props":{},"children":[{"type":"#text","text":"ready"}]}]');
                "##,
            )
            .expect("valid host commits must evaluate");
        assert_eq!(state.take_scene().expect("latest scene")[0].kind, "text");
    }

    #[test]
    fn checked_desktop_bundle_mounts_in_the_bounded_engine() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
        let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
        let desktop = fs::read(root.join("desktop/main.js")).expect("desktop bundle");
        let (host, state) = Host::new(Role::Desktop);
        let mut engine = Engine::open(Role::Desktop).expect("desktop engine must open");
        engine.install_host(host);
        engine
            .evaluate("runtime.js", &runtime)
            .expect("load runtime");
        engine.run_jobs().expect("runtime jobs");
        engine
            .evaluate("desktop.js", &desktop)
            .expect("mount desktop");
        engine.run_jobs().expect("desktop jobs");
        assert!(
            state.take_scene().is_some(),
            "desktop must publish its root"
        );
    }
}
