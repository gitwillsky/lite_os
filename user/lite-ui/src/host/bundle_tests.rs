//! Production-bundle integration tests: app/desktop bundle mounting, first-frame
//! asset resolution, scene bridge and host-boundary accelerator validation.

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
            engine.evaluate("timer.js", script.as_bytes()).expect("timer tick");
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
            // Alt+Tab, Alt+F4, Ctrl+Esc (mask bits Alt = 4, Ctrl = 2).
            display_proto::AcceleratorChord {
                modifiers: 4,
                code: 15
            },
            display_proto::AcceleratorChord {
                modifiers: 4,
                code: 62
            },
            display_proto::AcceleratorChord {
                modifiers: 2,
                code: 1
            },
        ]
    );
}

#[test]
fn accelerator_set_is_validated_at_the_host_boundary() {
    use quickjs_runtime::NativeHost;

    // Desktop session: a valid table queues one replacement action.
    let (mut desktop_host, state) = host(Role::Desktop, PathBuf::from("/"));
    desktop_host.invoke(
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
    assert!(desktop_host.invoke("desktop.accelerators.set", &overlong).is_err());
    assert!(desktop_host.invoke("desktop.accelerators.set", "not json").is_err());
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
