//! Fixed native operations exposed to the self-contained React bundle.

mod actions;
mod app_registry;
mod clipboard;
mod filesystem;
mod media;
mod scalar;
#[cfg(test)]
mod media_constants_tests;
#[cfg(test)]
mod media_controls_tests;

use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use quickjs_runtime::{EngineError, NativeHost, Role};
use serde::Serialize;

use scalar::{parse_i32, parse_u32, parse_u64};

use crate::{
    audio::Commands as AudioCommands,
    tree::{self, Node},
};

pub use actions::Action;

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
    scene_dirty: Cell<bool>,
    composition_dirty: Cell<bool>,
    actions: RefCell<Vec<Action>>,
    surfaces: RefCell<Vec<Surface>>,
    next_configure: Cell<u64>,
    focused_surface: Cell<u32>,
    timers: RefCell<Vec<(u64, Instant)>>,
    playback_granted: Cell<bool>,
}

impl State {
    /// Reports whether the latest React snapshot still needs one raster pass.
    pub fn scene_is_dirty(&self) -> bool {
        self.scene_dirty.get()
    }

    /// Reports whether unchanged desktop pixels need a fresh foreign-surface latch.
    pub fn composition_is_dirty(&self) -> bool {
        self.composition_dirty.get()
    }

    /// Consumes one pending foreign-surface-only scene latch.
    pub fn take_composition_dirty(&self) -> bool {
        self.composition_dirty.replace(false)
    }

    /// Requests a new scene latch without invalidating desktop raster pixels.
    pub fn invalidate_composition(&self) {
        self.composition_dirty.set(true);
    }

    /// Borrows the latest React host snapshot when it still needs rendering.
    ///
    /// The scene stays owned so a reconfigure can re-render it at the new
    /// viewport without a per-frame clone: [`State::invalidate_scene`] simply
    /// marks the retained snapshot dirty again.
    pub fn scene_if_dirty(&self) -> Option<std::cell::Ref<'_, Vec<Node>>> {
        if !self.scene_dirty.replace(false) {
            return None;
        }
        std::cell::Ref::filter_map(self.scene.borrow(), Option::as_ref).ok()
    }

    /// Borrows the retained React snapshot without consuming its dirty state.
    pub fn scene(&self) -> Option<std::cell::Ref<'_, Vec<Node>>> {
        std::cell::Ref::filter_map(self.scene.borrow(), Option::as_ref).ok()
    }

    /// Forces the retained snapshot to render once more (viewport change).
    pub fn invalidate_scene(&self) {
        self.scene_dirty.set(true);
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

    /// Records one compositor-authenticated physical input activation.
    pub fn grant_media_playback(&self) {
        self.playback_granted.set(true);
    }

    /// Adds one compositor-published app surface to desktop policy state.
    pub fn open_surface(&self, id: u32, app_id: String) {
        let index = self.surfaces.borrow().len() as u32;
        let (title, icon) = app_registry::app_metadata(&app_id);
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

    pub(crate) fn move_surface(&self, id: u32, x: u32, y: u32) -> Result<(), EngineError> {
        let mut surfaces = self.surfaces.borrow_mut();
        // A move can name a surface that just disconnected: the desktop's
        // resize/move commit references a window whose app closed mid-drag, one
        // React frame before the `closed` event prunes it (and native
        // MoveComplete can race the same way). The desktop reconciles on the
        // next AppClosed, so a missing surface here is a transient race, not
        // corruption — no-op instead of throwing (which, uncaught in JS, would
        // exit the whole desktop under panic=abort).
        let Some(surface) = surfaces.iter_mut().find(|surface| surface.id == id) else {
            eprintln!("lite-ui: move for unknown surface {id} dropped (disconnect race)");
            return Ok(());
        };
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
    app_root: PathBuf,
    files: RefCell<filesystem::Files>,
    audio: RefCell<AudioCommands>,
    next_media: Cell<u64>,
    next_clipboard_request: Cell<u64>,
    media: RefCell<BTreeSet<u64>>,
}

impl Host {
    /// Creates the unique host and its read-side state handle.
    pub fn new(role: Role, app_root: PathBuf, audio: AudioCommands) -> (Self, Rc<State>) {
        let state = Rc::new(State {
            scene: RefCell::new(None),
            scene_dirty: Cell::new(false),
            composition_dirty: Cell::new(false),
            actions: RefCell::new(Vec::new()),
            surfaces: RefCell::new(Vec::new()),
            next_configure: Cell::new(1),
            focused_surface: Cell::new(0),
            timers: RefCell::new(Vec::new()),
            playback_granted: Cell::new(false),
        });
        (
            Self {
                role,
                started: Instant::now(),
                state: state.clone(),
                app_root,
                files: RefCell::new(filesystem::Files::default()),
                audio: RefCell::new(audio),
                next_media: Cell::new(1),
                next_clipboard_request: Cell::new(1),
                media: RefCell::new(BTreeSet::new()),
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
        // `configure` is called during JSX render for every surface node, so it
        // can name a surface that disconnected one frame ago (before `reconcile`
        // prunes it). Emitting no Configure action and returning a benign serial
        // lets that final render complete; the surface vanishes next frame. A
        // throw here is uncaught in JS and would exit the whole desktop under
        // panic=abort — the very crash a rapid resize was triggering.
        let Some(surface) = surfaces.iter_mut().find(|surface| surface.id == surface_id) else {
            eprintln!(
                "lite-ui: configure for unknown surface {surface_id} skipped (disconnect race)"
            );
            return Ok("0".to_string());
        };
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
        if let Some(result) = self.invoke_media(operation, payload) {
            return result;
        }
        match operation {
            "scene.commit" => {
                let scene = tree::parse(payload).map_err(EngineError::from_host)?;
                self.state.scene.replace(Some(scene));
                self.state.scene_dirty.set(true);
                self.state.composition_dirty.set(false);
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
            "apps.list" if self.role == Role::Desktop => Ok(app_registry::scan_apps()),
            "apps.launch" if self.role == Role::Desktop && app_registry::valid_app_id(payload) => {
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
            "desktop.move.begin" if self.role == Role::Desktop => {
                let mut fields = payload.split(':');
                let surface_id = parse_u32(fields.next(), "moved surface")?;
                let serial = parse_u64(fields.next(), "pointer serial")?;
                let min_x = parse_i32(fields.next(), "minimum x")?;
                let min_y = parse_i32(fields.next(), "minimum y")?;
                let max_x = parse_i32(fields.next(), "maximum x")?;
                let max_y = parse_i32(fields.next(), "maximum y")?;
                if fields.next().is_some() || max_x < min_x || max_y < min_y {
                    return Err(EngineError::from_host("invalid desktop move bounds"));
                }
                self.state.actions.borrow_mut().push(Action::BeginMove {
                    surface_id,
                    serial,
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                });
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
            // The pre-first-update frame mirrors the helper's initial palette
            // (terminal-session DEFAULT_COLORS indices 7 and 0); the first real
            // screen update replaces both colors and rows.
            "terminal.connect" if self.role == Role::App => Ok(
                r#"{"rows":[[{"text":"Connecting to LiteOS terminal...","fg":13358561,"bg":1053720,"bold":false}]],"cursor":{"column":0,"row":0},"foreground":13358561,"background":1053720}"#.to_owned(),
            ),
            "terminal.input" if self.role == Role::App => {
                self.state.actions.borrow_mut().push(Action::TerminalInput(payload.as_bytes().to_vec()));
                Ok(String::new())
            }
            "terminal.paste" if self.role == Role::App => self.terminal_paste(payload),
            "clipboard.read" => self.clipboard_read(),
            "clipboard.write" => self.clipboard_write(payload),
            "fs.list" if self.role == Role::App => Ok(filesystem::list(payload)),
            "fs.read" if self.role == Role::App => Ok(filesystem::read(payload)),
            "fs.mkdir" if self.role == Role::App => Ok(filesystem::mkdir(payload)),
            "fs.remove" if self.role == Role::App => Ok(filesystem::remove(payload)),
            "fs.rename" if self.role == Role::App => Ok(filesystem::rename(payload)),
            "fs.copy" if self.role == Role::App => Ok(filesystem::copy(payload)),
            _ => Err(EngineError::from_host(format!(
                "operation '{operation}' is unavailable in this session"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use quickjs_runtime::{Engine, Role};

    use super::Host;

    fn host(role: Role, root: PathBuf) -> (Host, std::rc::Rc<super::State>) {
        let audio_role = if role == Role::Desktop {
            audio_proto::ClientRole::Desktop
        } else {
            audio_proto::ClientRole::Media
        };
        let (commands, _events) = crate::audio::start(audio_role).expect("audio worker");
        Host::new(role, root, commands)
    }

    /// Mounts one app bundle like the production session and asserts every
    /// `<img src>` in its first frame resolves to a real file under the app
    /// root — the exact failure a missing build.mjs asset copy causes at
    /// first paint in the guest.
    fn assert_app_bundle_mounts_with_assets(app: &str) {
        let root = std::env::var_os("LITE_UI_TEST_ASSETS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
        let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
        let bundle = fs::read(root.join(app).join("main.js")).expect("app bundle");
        let app_root = root.join(app);
        let (host, state) = host(Role::App, app_root.clone());
        let mut engine = Engine::open(Role::App).expect("app engine");
        engine.install_host(host);
        engine
            .evaluate("runtime.js", &runtime)
            .expect("load runtime");
        engine.run_jobs().expect("runtime jobs");
        engine
            .evaluate("app.js", &bundle)
            .expect("mount app");
        engine.run_jobs().expect("app jobs");
        let scene = state.scene_if_dirty().expect("app must publish its root");
        let mut stack: Vec<&crate::tree::Node> = scene.iter().collect();
        let mut srcs = Vec::new();
        while let Some(node) = stack.pop() {
            if node.kind == "img"
                && let Some(src) = node.props.get("src").and_then(serde_json::Value::as_str)
            {
                srcs.push(src.to_owned());
            }
            stack.extend(node.children.iter());
        }
        assert!(!srcs.is_empty(), "first frame should reference assets");
        for src in srcs {
            assert!(
                app_root.join(&src).is_file(),
                "first-frame asset missing from bundle: {app}/{src}"
            );
        }
    }

    #[test]
    fn explorer_apps_mount_with_all_first_frame_assets() {
        assert_app_bundle_mounts_with_assets("my-computer");
        assert_app_bundle_mounts_with_assets("file-manager");
    }

    #[test]
    fn quickjs_bridge_publishes_only_the_latest_complete_scene() {
        let (host, state) = host(Role::Desktop, PathBuf::from("/"));
        let mut engine = Engine::open(Role::Desktop).expect("desktop engine must open");
        engine.install_host(host);
        engine
            .evaluate(
                "host.js",
                br##"
                __liteNative("scene.commit", '[{"id":1,"type":"div","props":{},"children":[]}]');
                __liteNative("scene.commit", '[{"id":2,"type":"span","props":{},"children":[{"id":3,"type":"#text","text":"ready"}]}]');
                "##,
            )
            .expect("valid host commits must evaluate");
        assert_eq!(
            state.scene_if_dirty().expect("latest scene")[0].kind,
            "span"
        );
        // The dirty flag is consumed by the read: a second poll sees no work,
        // and an explicit invalidation offers the same retained scene again.
        assert!(state.scene_if_dirty().is_none());
        state.invalidate_scene();
        assert!(state.scene_if_dirty().is_some());
    }

    #[test]
    fn checked_desktop_bundle_mounts_in_the_bounded_engine() {
        let root = std::env::var_os("LITE_UI_TEST_ASSETS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
        let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
        let desktop = fs::read(root.join("desktop/main.js")).expect("desktop bundle");
        let (host, state) = host(Role::Desktop, root.clone());
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
            state.scene_if_dirty().is_some(),
            "desktop must publish its root"
        );
    }

    #[test]
    fn audible_media_requires_physical_activation_and_system_audio_is_desktop_only() {
        let (host, state) = host(Role::App, PathBuf::from("/"));
        let mut engine = Engine::open(Role::App).expect("app engine");
        engine.install_host(host);
        engine
            .evaluate(
                "create.js",
                br#"globalThis.mediaId = Number(__liteNative("media.create", "{}"));"#,
            )
            .expect("create media");
        assert!(
            engine
                .evaluate(
                    "denied.js",
                    br#"__liteNative("media.play", JSON.stringify({id:mediaId,muted:false}));"#,
                )
                .is_err(),
            "script cannot synthesize an audible playback grant"
        );
        engine
            .evaluate(
                "muted.js",
                br#"__liteNative("media.gain", JSON.stringify({id:mediaId,volume:1,muted:true}));"#,
            )
            .expect("muted playback state does not require activation");
        assert!(
            engine
                .evaluate(
                    "unmute.js",
                    br#"__liteNative("media.gain", JSON.stringify({id:mediaId,volume:1,muted:false}));"#,
                )
                .is_err(),
            "unmuting without activation must remain atomic at the host boundary"
        );
        state.grant_media_playback();
        assert!(
            engine
                .evaluate("system.js", br#"__liteNative("audio-system.get", "");"#)
                .is_err(),
            "ordinary app cannot acquire desktop system-volume capability"
        );
    }

}
