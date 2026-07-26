use std::{fs, path::PathBuf};

use quickjs_runtime::{Engine, Role};

use super::Host;

#[test]
fn public_media_instance_exposes_standard_state_and_error_constants() {
    let root = std::env::var_os("LITE_UI_TEST_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));
    let runtime = fs::read(root.join("runtime.js")).expect("runtime bundle");
    let app_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/audio");
    let (commands, _events) =
        crate::audio::start(audio_proto::ClientRole::Media).expect("audio worker");
    let (host, _state) = Host::new(Role::App, app_root, commands);
    let mut engine = Engine::open(Role::App).expect("app engine");
    engine.install_host(host);
    engine.evaluate("runtime.js", &runtime).expect("runtime");
    engine.run_jobs().expect("runtime jobs");
    engine
        .evaluate(
            "media-constants.js",
            br#"
            globalThis.mediaSurface = null;
            __liteMount(() => __liteReact.createElement("audio", {
                ref(value) { if (value) globalThis.mediaSurface = value; }
            }));
            "#,
        )
        .expect("mount public audio element");
    engine.run_jobs().expect("media mount jobs");
    engine
        .evaluate(
            "assert-media-constants.js",
            br#"
            if (!mediaSurface
                || mediaSurface.NETWORK_EMPTY !== 0
                || mediaSurface.NETWORK_IDLE !== 1
                || mediaSurface.NETWORK_LOADING !== 2
                || mediaSurface.NETWORK_NO_SOURCE !== 3
                || mediaSurface.HAVE_NOTHING !== 0
                || mediaSurface.HAVE_METADATA !== 1
                || mediaSurface.HAVE_CURRENT_DATA !== 2
                || mediaSurface.HAVE_FUTURE_DATA !== 3
                || mediaSurface.HAVE_ENOUGH_DATA !== 4
                || MediaError.MEDIA_ERR_ABORTED !== 1
                || MediaError.MEDIA_ERR_NETWORK !== 2
                || MediaError.MEDIA_ERR_DECODE !== 3
                || MediaError.MEDIA_ERR_SRC_NOT_SUPPORTED !== 4) {
                throw new Error("public media constants do not match the Web contract");
            }
            __liteEvent("media", {
                id: mediaSurface.id, type: "durationchange", duration: 2
            });
            __liteEvent("media", { id: mediaSurface.id, type: "loadeddata" });
            if (mediaSurface.buffered.length !== 1
                || mediaSurface.buffered.start(0) !== 0
                || mediaSurface.buffered.end(0) !== 2) {
                throw new Error("local buffered range does not cover the decoded resource");
            }
            for (const index of [-1, 1, 0.5]) {
                try {
                    mediaSurface.buffered.start(index);
                    throw new Error("invalid TimeRanges index was accepted");
                } catch (error) {
                    if (error.name !== "IndexSizeError") throw error;
                }
            }
            __liteEvent("media", { id: mediaSurface.id, type: "abort" });
            if (mediaSurface.buffered.length !== 0) {
                throw new Error("abort retained a stale buffered range");
            }
            __liteEvent("media", { id: mediaSurface.id, type: "canplay" });
            __liteEvent("media", {
                id: mediaSurface.id, type: "error",
                error: { code: 3, message: "decode failed" }
            });
            if (mediaSurface.buffered.length !== 0) {
                throw new Error("error retained a stale buffered range");
            }
            const native = globalThis.__liteNative;
            const operations = [];
            globalThis.__liteNative = (operation, payload) => {
                operations.push(operation);
                return native(operation, payload);
            };
            mediaSurface.src = "tone.wav";
            mediaSurface.src = "";
            if (operations.at(-1) !== "media.unload") {
                throw new Error("empty src did not close the loaded worker source");
            }
            "#,
        )
        .expect("standard media constants");
}
