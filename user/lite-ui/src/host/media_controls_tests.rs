use std::{fs, path::PathBuf};

use quickjs_runtime::{Engine, Role};

use crate::tree::Node;

use super::Host;

fn collect_text(nodes: &[Node], output: &mut Vec<String>) {
    for node in nodes {
        if node.kind == "#text" {
            output.push(node.text.clone());
        }
        collect_text(&node.children, output);
    }
}

fn count_range_inputs(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|node| {
            usize::from(
                node.kind == "input"
                    && node.props.get("type").and_then(serde_json::Value::as_str) == Some("range"),
            ) + count_range_inputs(&node.children)
        })
        .sum()
}

#[test]
fn production_music_player_uses_standard_range_and_readable_media_controls() {
    let root = std::env::var_os("LITE_UI_TEST_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
    let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
    let player = fs::read(root.join("music-player/main.js")).expect("music bundle");
    let runtime_text = std::str::from_utf8(&runtime).expect("runtime bundle is UTF-8");
    let player_text = std::str::from_utf8(&player).expect("music bundle is UTF-8");
    for label in [
        "Play", "Pause", "-10", "+10", "Mute", "Unmute", "Vol -", "Vol +",
    ] {
        assert!(
            runtime_text.contains(label),
            "runtime bundle omitted ASCII media label {label:?}"
        );
    }
    for label in [
        "Back to Library",
        "Previous",
        "Play",
        "Pause",
        "Next",
        "Audio Settings",
        "Open Folder",
    ] {
        assert!(
            player_text.contains(label),
            "music bundle omitted ASCII control label {label:?}"
        );
    }
    for unsupported in ['▶', 'Ⅱ', '🔇', '🔊', '−'] {
        assert!(
            !runtime_text.contains(unsupported) && !player_text.contains(unsupported),
            "production bundles retained unsupported media glyph {unsupported:?}"
        );
    }

    let (commands, _events) =
        crate::audio::start(audio_proto::ClientRole::Media).expect("audio worker");
    let app_root = root.join("music-player");
    let (host, state) = Host::new(
        Role::App,
        app_root.clone(),
        root,
        commands,
        display_proto::Size {
            width: 1504,
            height: 846,
        },
    );
    let mut engine = Engine::open(Role::App).expect("app engine");
    engine.install_host(host);
    engine.evaluate("runtime.js", &runtime).expect("runtime");
    engine.run_jobs().expect("runtime jobs");
    engine.evaluate("music-player.js", &player).expect("player");
    engine.run_jobs().expect("player jobs");
    let scene = state.scene_if_dirty().expect("player publishes a scene");
    let mut text = Vec::new();
    collect_text(&scene, &mut text);
    for label in [
        "Back to Library",
        "Previous",
        "Play",
        "Next",
        "Audio Settings",
        "Open Folder",
        "Mute",
    ] {
        assert!(
            text.iter().any(|value| value == label),
            "production media surface omitted ASCII control label {label:?}"
        );
    }
    assert!(
        text.iter().all(|value| !value
            .chars()
            .any(|character| matches!(character, '▶' | 'Ⅱ' | '🔇' | '🔊' | '−'))),
        "production media controls retained a glyph outside the fixed UI font"
    );
    assert_eq!(
        count_range_inputs(&scene),
        2,
        "now-playing surface must expose seek and volume as standard range inputs"
    );
}
