//! Production-bundle integration tests: app/desktop bundle mounting, first-frame
//! asset resolution, scene bridge and host-boundary accelerator validation.

use std::{fs, path::PathBuf};

use quickjs_runtime::{Engine, EngineError, Role};

use super::{Action, ExtensionCx, Host, HostExtension};

/// Desktop-policy stand-in for the bundle tests. The real desktop policy
/// (`apps.list`/`apps.launch`/`desktop.shutdown`) lives in the `desktop` binary
/// crate as a `HostExtension`; the library cannot depend on that crate, so this
/// mirrors the same three ops (validating the extension seam end to end: the JS
/// bundle invokes them and the produced `Action`s reach the run loop's queue).
struct DesktopTestExt;

impl HostExtension for DesktopTestExt {
    fn invoke(
        &mut self,
        cx: &ExtensionCx,
        operation: &str,
        payload: &str,
    ) -> Option<Result<String, EngineError>> {
        match operation {
            // Two shipped bundles are enough for the launcher-driven tests.
            "apps.list" => Some(Ok(r#"[{"id":"music-player","name":"Music","description":"","icon":"assets/monitor.png"},{"id":"terminal","name":"Terminal","description":"","icon":"assets/terminal.png"}]"#.to_owned())),
            "apps.launch" => {
                cx.push_action(Action::Launch(payload.to_owned()));
                Some(Ok(String::new()))
            }
            "desktop.shutdown" => {
                cx.push_action(Action::Shutdown);
                Some(Ok(String::new()))
            }
            _ => None,
        }
    }
}

fn host(role: Role, root: PathBuf) -> (Host, std::rc::Rc<super::State>) {
    let audio_role = if role == Role::Desktop {
        audio_proto::ClientRole::Desktop
    } else {
        audio_proto::ClientRole::Media
    };
    let (commands, _events) = crate::audio::start(audio_role).expect("audio worker");
    // The desktop bundle calls the desktop-policy ops on mount, so the desktop
    // role installs the policy stand-in; app roles carry no extensions.
    let extensions: Vec<Box<dyn HostExtension>> = if role == Role::Desktop {
        vec![Box::new(DesktopTestExt)]
    } else {
        Vec::new()
    };
    Host::new(
        role,
        root.clone(),
        root,
        commands,
        extensions,
        display_proto::Size {
            width: 1504,
            height: 846,
        },
    )
}

/// Mounts one app bundle like the production session and asserts every
/// `<img src>` that exists in its first frame resolves under the app root.
///
/// Returns the resolved sources so callers whose initial state necessarily
/// contains imagery can additionally require a non-empty set. Files may render
/// its CSS-only empty-directory shell before filesystem entries introduce
/// images, so an unconditional non-empty requirement would encode old chrome.
fn assert_app_bundle_mounts_with_assets(app: &str) -> Vec<String> {
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
    engine.evaluate("app.js", &bundle).expect("mount app");
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
    for src in &srcs {
        assert!(
            app_root.join(src).is_file(),
            "first-frame asset missing from bundle: {app}/{src}"
        );
    }
    srcs
}

#[test]
fn explorer_apps_mount_with_all_first_frame_assets() {
    assert!(
        !assert_app_bundle_mounts_with_assets("my-computer").is_empty(),
        "Computer's first frame must exercise its packaged imagery"
    );
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
fn runtime_exposes_standard_retina_viewport_and_resize_event() {
    let root = std::env::var_os("LITE_UI_TEST_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
    let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
    let (host, _state) = host(Role::Desktop, root);
    let mut engine = Engine::open(Role::Desktop).expect("desktop engine");
    engine.install_host(host);
    engine
        .evaluate("runtime.js", &runtime)
        .expect("load runtime");
    engine
        .evaluate(
            "viewport-test.js",
            br#"
            if (window !== globalThis || innerWidth !== 1504 || innerHeight !== 846 || devicePixelRatio !== 2) {
              throw new Error("initial viewport contract failed");
            }
            let resizeCount = 0;
            const onResize = () => { resizeCount += 1; };
            window.addEventListener("resize", onResize);
            __liteEvent("viewport", { width: 1280, height: 720, devicePixelRatio: 2 });
            window.removeEventListener("resize", onResize);
            if (innerWidth !== 1280 || innerHeight !== 720 || resizeCount !== 1) {
              throw new Error("resize event contract failed");
            }
            "#,
        )
        .expect("standard viewport must update");
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
    let scene = state
        .scene_if_dirty()
        .expect("desktop must publish its root");
    let mut stack: Vec<&crate::tree::Node> = scene.iter().collect();
    let mut splash_assets = Vec::new();
    while let Some(node) = stack.pop() {
        if node.kind == "img"
            && let Some(src) = node.props.get("src").and_then(serde_json::Value::as_str)
            && src.starts_with("assets/aurora-")
        {
            splash_assets.push(src.to_owned());
        }
        stack.extend(node.children.iter());
    }
    splash_assets.sort();
    assert_eq!(
        splash_assets,
        ["assets/aurora-background.png", "assets/aurora-logo.png"],
        "the first desktop scene must own both approved splash layers"
    );
    for src in splash_assets {
        assert!(
            root.join("desktop").join(src).is_file(),
            "desktop splash asset missing from production bundle"
        );
    }
}

#[test]
fn checked_desktop_listener_dispatch_commits_react_state() {
    let root = std::env::var_os("LITE_UI_TEST_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
    let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
    let desktop = fs::read(root.join("desktop/main.js")).expect("desktop bundle");
    let (host, state) = host(Role::Desktop, root);
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
    let scene = state.scene_if_dirty().expect("desktop root");
    let mut stack: Vec<&crate::tree::Node> = scene.iter().collect();
    let mut listener = None;
    while let Some(node) = stack.pop() {
        if node
            .props
            .get("className")
            .and_then(serde_json::Value::as_str)
            == Some("workspace-switcher")
        {
            listener = node
                .props
                .get("onClick")
                .and_then(serde_json::Value::as_u64);
            break;
        }
        stack.extend(node.children.iter());
    }
    let listener = listener.expect("workspace switcher listener");
    drop(scene);
    for (surface_id, app_id) in [(1, "terminal"), (2, "file-manager")] {
        state.open_surface(surface_id, app_id.to_owned());
        engine
            .evaluate(
                "surface.js",
                format!(
                    "globalThis.__liteEvent(\"desktop\",{{\"type\":\"opened\",\"surface\":{{\"id\":{surface_id},\"appId\":\"{app_id}\"}}}});"
                )
                .as_bytes(),
            )
            .expect("dispatch production surface event");
        engine.run_jobs().expect("surface jobs");
    }
    drop(
        state
            .scene_if_dirty()
            .expect("surface events must publish windows"),
    );
    engine
        .evaluate(
            "listener.js",
            format!("globalThis.__liteDispatch([{listener}],{{\"type\":\"click\"}});").as_bytes(),
        )
        .expect("dispatch workspace click");
    engine.run_jobs().expect("listener jobs");
    let scene = state
        .scene_if_dirty()
        .expect("listener must publish a React commit");
    let mut stack: Vec<&crate::tree::Node> = scene.iter().collect();
    while let Some(node) = stack.pop() {
        if node
            .props
            .get("className")
            .and_then(serde_json::Value::as_str)
            == Some("overview")
        {
            return;
        }
        stack.extend(node.children.iter());
    }
    panic!("workspace listener did not render the overview");
}

#[test]
fn close_control_waits_for_app_closed_without_starting_a_move() {
    let root = std::env::var_os("LITE_UI_TEST_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
    let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
    let desktop = fs::read(root.join("desktop/main.js")).expect("desktop bundle");
    let (host, state) = host(Role::Desktop, root);
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
    drop(state.scene_if_dirty().expect("desktop root"));

    state.open_surface(7, "terminal".to_owned());
    engine
        .evaluate(
            "open-terminal.js",
            br#"globalThis.__liteEvent("desktop",{"type":"opened","surface":{"id":7,"appId":"terminal"}});"#,
        )
        .expect("dispatch opened event");
    engine.run_jobs().expect("opened jobs");
    let scene = state
        .scene_if_dirty()
        .expect("opened surface must publish one window");
    let nodes: Vec<_> = scene
        .iter()
        .flat_map(|node| descendants(node).into_iter())
        .collect();
    let controls_down = nodes
        .iter()
        .find(|node| {
            node.props
                .get("className")
                .and_then(serde_json::Value::as_str)
                == Some("window__controls")
        })
        .and_then(|node| node.props.get("onPointerDown"))
        .and_then(serde_json::Value::as_u64)
        .expect("window controls must isolate pointer-down from the titlebar");
    let titlebar_down = nodes
        .iter()
        .find(|node| {
            node.props
                .get("className")
                .and_then(serde_json::Value::as_str)
                == Some("window__titlebar")
        })
        .and_then(|node| node.props.get("onPointerDown"))
        .and_then(serde_json::Value::as_u64)
        .expect("titlebar pointer-down listener");
    let window_down = nodes
        .iter()
        .find(|node| {
            node.props
                .get("data-lite-window")
                .and_then(serde_json::Value::as_u64)
                == Some(7)
        })
        .and_then(|node| node.props.get("onPointerDown"))
        .and_then(serde_json::Value::as_u64)
        .expect("window activation listener");
    let close_click = nodes
        .iter()
        .find(|node| {
            node.props
                .get("aria-label")
                .and_then(serde_json::Value::as_str)
                == Some("Close")
        })
        .and_then(|node| node.props.get("onClick"))
        .and_then(serde_json::Value::as_u64)
        .expect("close click listener");
    drop(nodes);
    drop(scene);
    state.take_actions();

    engine
        .evaluate(
            "close-pointer-down.js",
            format!(
                "globalThis.__liteDispatch([{controls_down},{titlebar_down},{window_down}],{{\"type\":\"pointer\",\"phase\":\"down\",\"serial\":41}});"
            )
            .as_bytes(),
        )
        .expect("dispatch close pointer-down route");
    engine.run_jobs().expect("pointer-down jobs");
    assert!(
        !state
            .take_actions()
            .iter()
            .any(|action| matches!(action, Action::BeginMove { surface_id: 7, .. })),
        "window controls must not enter the titlebar move owner"
    );

    engine
        .evaluate(
            "close-click.js",
            format!("globalThis.__liteDispatch({close_click},{{\"type\":\"click\"}});").as_bytes(),
        )
        .expect("dispatch close click");
    engine.run_jobs().expect("close click jobs");
    let actions = state.take_actions();
    assert_eq!(
        actions
            .iter()
            .filter(|action| matches!(action, Action::Close(7)))
            .count(),
        1,
        "close control must publish exactly one close request"
    );
    state.invalidate_scene();
    let scene = state
        .scene_if_dirty()
        .expect("close request retains the native-owned surface");
    assert!(
        has_window(&scene, 7),
        "close request must wait for AppClosed instead of hiding optimistically"
    );
    drop(scene);

    state
        .move_surface(7, 320, 180)
        .expect("delayed native move remains valid before AppClosed");
    engine
        .evaluate(
            "delayed-move.js",
            br#"globalThis.__liteEvent("desktop",{"type":"moved","surfaceId":7,"x":320,"y":180});"#,
        )
        .expect("dispatch delayed move completion");
    engine.run_jobs().expect("delayed move jobs");
    let scene = state
        .scene_if_dirty()
        .expect("delayed move publishes retained window");
    assert!(
        has_window(&scene, 7),
        "an event before AppClosed must not toggle surface existence"
    );
    drop(scene);

    state.close_surface(7);
    engine
        .evaluate(
            "app-closed.js",
            br#"globalThis.__liteEvent("desktop",{"type":"closed","surfaceId":7});"#,
        )
        .expect("dispatch AppClosed");
    engine.run_jobs().expect("AppClosed jobs");
    let scene = state
        .scene_if_dirty()
        .expect("AppClosed must publish the removal");
    assert!(
        !has_window(&scene, 7),
        "AppClosed is the sole transition that removes the window"
    );
}

#[test]
fn command_launch_unmounts_the_panel_in_the_same_react_commit() {
    let root = std::env::var_os("LITE_UI_TEST_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
    let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
    let desktop = fs::read(root.join("desktop/main.js")).expect("desktop bundle");
    let (host, state) = host(Role::Desktop, root);
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

    let scene = state.scene_if_dirty().expect("desktop root");
    let brand_listener = scene
        .iter()
        .flat_map(|node| descendants(node).into_iter())
        .find(|node| {
            node.props
                .get("className")
                .and_then(serde_json::Value::as_str)
                == Some("topbar__brand")
        })
        .and_then(|node| node.props.get("onClick"))
        .and_then(serde_json::Value::as_u64)
        .expect("topbar brand listener");
    drop(scene);
    engine
        .evaluate(
            "open-command.js",
            format!("globalThis.__liteDispatch([{brand_listener}],{{\"type\":\"click\"}});")
                .as_bytes(),
        )
        .expect("open command center");
    engine.run_jobs().expect("command center jobs");

    let scene = state.scene_if_dirty().expect("command center scene");
    let music_listener = scene
        .iter()
        .flat_map(|node| descendants(node).into_iter())
        .find(|node| {
            node.props
                .get("className")
                .and_then(serde_json::Value::as_str)
                == Some("command-app")
                && descendants(node).into_iter().any(|child| {
                    child.props.get("src").and_then(serde_json::Value::as_str)
                        == Some("assets/monitor.png")
                })
        })
        .and_then(|node| node.props.get("onClick"))
        .and_then(serde_json::Value::as_u64)
        .expect("Music command listener");
    drop(scene);
    engine
        .evaluate(
            "launch-music.js",
            format!("globalThis.__liteDispatch([{music_listener}],{{\"type\":\"click\"}});")
                .as_bytes(),
        )
        .expect("launch Music");
    engine.run_jobs().expect("launch jobs");

    assert!(
        state
            .take_actions()
            .iter()
            .any(|action| { matches!(action, super::Action::Launch(id) if id == "music-player") }),
        "Music command must publish the production launch action"
    );
    let scene = state
        .scene_if_dirty()
        .expect("launch must publish the panel-close commit");
    assert!(
        !scene
            .iter()
            .flat_map(|node| descendants(node).into_iter())
            .any(|node| {
                node.props
                    .get("className")
                    .and_then(serde_json::Value::as_str)
                    == Some("command-center")
            }),
        "the command panel must unmount in the same discrete event"
    );
}

fn descendants(node: &crate::tree::Node) -> Vec<&crate::tree::Node> {
    let mut nodes = vec![node];
    for child in &node.children {
        nodes.extend(descendants(child));
    }
    nodes
}

fn has_window(scene: &[crate::tree::Node], surface_id: u64) -> bool {
    scene.iter().any(|node| {
        descendants(node).into_iter().any(|node| {
            node.props
                .get("data-lite-window")
                .and_then(serde_json::Value::as_u64)
                == Some(surface_id)
        })
    })
}

/// Mounts the production desktop bundle and fires the passive-effect
/// timers the runtime scheduled, so startup effects run exactly as the
/// event loop drives them in the guest.
#[test]
fn checked_desktop_bundle_registers_its_accelerator_chords() {
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
    // Passive effects are scheduled through the host timer bridge; fire the
    // expired ones like the main loop does until no new timer appears.
    for _ in 0..8 {
        let expired = state.take_expired_timers();
        if expired.is_empty() {
            break;
        }
        for id in expired {
            let script = format!("globalThis.__liteTimer({id});");
            engine
                .evaluate("timer.js", script.as_bytes())
                .expect("timer tick");
            engine.run_jobs().expect("timer jobs");
        }
    }
    let chords = state
        .take_actions()
        .into_iter()
        .find_map(|action| match action {
            super::Action::SetAccelerators(chords) => Some(chords),
            _ => None,
        })
        .expect("desktop startup must register its accelerator table");
    assert_eq!(
        chords,
        [
            // Escape dismisses global panels; Alt uses modifier-mask bit 4.
            display_proto::AcceleratorChord {
                modifiers: 0,
                code: 1
            },
            display_proto::AcceleratorChord {
                modifiers: 4,
                code: 15
            },
            display_proto::AcceleratorChord {
                modifiers: 4,
                code: 62
            },
        ]
    );
}

#[test]
fn accelerator_set_is_validated_at_the_host_boundary() {
    use quickjs_runtime::NativeHost;

    // Desktop session: a valid table queues one replacement action.
    let (mut desktop_host, state) = host(Role::Desktop, PathBuf::from("/"));
    desktop_host
        .invoke(
            "desktop.accelerators.set",
            r#"[{"modifiers":4,"code":15},{"modifiers":2,"code":1}]"#,
        )
        .expect("valid accelerator table");
    let actions = state.take_actions();
    let [super::Action::SetAccelerators(chords)] = actions.as_slice() else {
        panic!("one accelerator replacement expected");
    };
    assert_eq!(
        chords,
        &[
            display_proto::AcceleratorChord {
                modifiers: 4,
                code: 15
            },
            display_proto::AcceleratorChord {
                modifiers: 2,
                code: 1
            },
        ]
    );
    // Overlong tables are rejected before any wire encode, and a malformed
    // payload never queues an action.
    let overlong = format!(
        "[{}]",
        (0..=display_proto::MAX_ACCELERATORS)
            .map(|_| r#"{"modifiers":4,"code":15}"#)
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(
        desktop_host
            .invoke("desktop.accelerators.set", &overlong)
            .is_err()
    );
    assert!(
        desktop_host
            .invoke("desktop.accelerators.set", "not json")
            .is_err()
    );
    assert!(state.take_actions().is_empty());
    // App sessions never reach the handler.
    let (mut app_host, _) = host(Role::App, PathBuf::from("/"));
    assert!(app_host.invoke("desktop.accelerators.set", "[]").is_err());
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
