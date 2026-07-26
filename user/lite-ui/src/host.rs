//! Fixed native operations exposed to the self-contained React bundle.

mod filesystem;
mod media;
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
use serde::{Deserialize, Serialize};

use crate::{
    audio::Commands as AudioCommands,
    tree::{self, Node},
};

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
    /// Authorize one compositor-side move from an exact pointer-down serial.
    BeginMove {
        surface_id: u32,
        serial: u64,
        min_x: i32,
        min_y: i32,
        max_x: i32,
        max_y: i32,
    },
    /// Request system shutdown.
    Shutdown,
    /// Send bytes to the terminal helper.
    TerminalInput(Vec<u8>),
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
            "apps.list" if self.role == Role::Desktop => Ok(scan_apps()),
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

fn app_metadata(id: &str) -> (&'static str, &'static str) {
    match id {
        "terminal" => ("Terminal", "assets/terminal.png"),
        "my-computer" => ("我的电脑", "assets/computer.png"),
        "file-manager" => ("File Manager", "assets/computer.png"),
        "music-player" => ("Music Player", "assets/speaker.png"),
        _ => ("Application", "assets/terminal.png"),
    }
}

// Installed application bundles. Each `<id>/app.json` is one launchable app;
// `apps.launch` spawns `/bin/lite-ui --app <id>` against `<APPS_ROOT>/<id>`.
const APPS_ROOT: &str = "/usr/share/liteos/apps";
// The desktop bundle ships exactly these icons (see ui/build.mjs). The start
// menu renders under the desktop role, so `src="assets/<name>"` only resolves
// against the desktop root — a manifest icon outside this set cannot load, so
// it is normalized to a shipped name (or the fallback) rather than trusted.
const DESKTOP_ICON_NAMES: [&str; 5] = [
    "computer.png",
    "terminal.png",
    "documents.png",
    "trash.png",
    "speaker.png",
];
const FALLBACK_ICON: &str = "assets/terminal.png";

/// On-disk per-app manifest (`<id>/app.json`). Extra fields (entry/style) are
/// ignored here; only what the launcher chrome needs is deserialized.
#[derive(Deserialize)]
struct AppManifest {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    icon: Option<String>,
}

/// The launcher registry entry the desktop React consumes (matches AppMeta in
/// ui/types/lite.d.ts).
#[derive(Serialize)]
struct AppMeta {
    id: String,
    name: String,
    description: String,
    icon: String,
}

/// Constrains a manifest icon to an asset the desktop bundle actually ships,
/// falling back to the terminal icon so the start menu never renders a missing
/// image (mirrors `app_metadata`'s fallback).
fn normalize_icon(icon: Option<&str>) -> String {
    let name = icon
        .and_then(|value| value.rsplit('/').next())
        .filter(|name| DESKTOP_ICON_NAMES.contains(name));
    match name {
        Some(name) => format!("assets/{name}"),
        None => FALLBACK_ICON.to_owned(),
    }
}

/// Enumerates `<APPS_ROOT>/*/app.json` into the launcher registry. Unreadable
/// directories, missing/malformed manifests, and ids that fail `valid_app_id`
/// are skipped rather than fatal, so one bad bundle never blanks the menu.
/// Results are sorted by id for a deterministic render order.
fn scan_apps() -> String {
    let mut apps: Vec<AppMeta> = match std::fs::read_dir(APPS_ROOT) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let manifest = std::fs::read_to_string(entry.path().join("app.json")).ok()?;
                let manifest: AppManifest = serde_json::from_str(&manifest).ok()?;
                valid_app_id(&manifest.id).then_some(())?;
                Some(AppMeta {
                    icon: normalize_icon(manifest.icon.as_deref()),
                    id: manifest.id,
                    name: manifest.name,
                    description: manifest.description,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    apps.sort_by(|a, b| a.id.cmp(&b.id));
    serde_json::to_string(&apps).unwrap_or_else(|_| "[]".to_owned())
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

fn parse_i32(value: Option<&str>, name: &str) -> Result<i32, EngineError> {
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

    fn host(role: Role, root: PathBuf) -> (Host, std::rc::Rc<super::State>) {
        let audio_role = if role == Role::Desktop {
            audio_proto::ClientRole::Desktop
        } else {
            audio_proto::ClientRole::Media
        };
        let (commands, _events) = crate::audio::start(audio_role).expect("audio worker");
        Host::new(role, root, commands)
    }

    #[test]
    fn scratch_my_computer_mounts() {
        let root = std::env::var_os("LITE_UI_TEST_ASSETS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
        let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
        let bundle = fs::read(root.join("my-computer/main.js")).expect("my-computer bundle");
        let (host, state) = host(Role::App, root.join("my-computer"));
        let mut engine = Engine::open(Role::App).expect("app engine");
        engine.install_host(host);
        engine
            .evaluate("runtime.js", &runtime)
            .expect("load runtime");
        engine.run_jobs().expect("runtime jobs");
        engine
            .evaluate("mc.js", &bundle)
            .expect("mount my-computer");
        engine.run_jobs().expect("my-computer jobs");
        assert!(
            state.scene_if_dirty().is_some(),
            "my-computer must publish its root"
        );
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

    #[test]
    fn manifest_icon_is_constrained_to_shipped_desktop_assets() {
        // A shipped name survives, path prefixes are stripped to the basename.
        assert_eq!(super::normalize_icon(Some("assets/speaker.png")), "assets/speaker.png");
        assert_eq!(super::normalize_icon(Some("speaker.png")), "assets/speaker.png");
        // Anything the desktop bundle does not ship, or a missing icon, falls
        // back so the start menu never references an unresolvable image.
        assert_eq!(super::normalize_icon(Some("assets/custom.png")), super::FALLBACK_ICON);
        assert_eq!(super::normalize_icon(None), super::FALLBACK_ICON);
    }
}
