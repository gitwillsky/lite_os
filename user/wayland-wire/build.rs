//! 把 `protocols/*.xml` 经 `wayland-scanner` 生成为 `$OUT_DIR/protocol.rs`。

use std::{env, fs, path::PathBuf};

fn main() {
    let directory = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("protocols");
    println!("cargo:rerun-if-changed={}", directory.display());
    let mut paths: Vec<_> = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "xml"))
        .collect();
    // 文件名顺序决定 `Interface` 枚举顺序，固定排序让生成结果可复现。
    paths.sort();
    let sources: Vec<String> = paths
        .iter()
        .map(|path| {
            fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        })
        .collect();
    let sources: Vec<&str> = sources.iter().map(String::as_str).collect();
    let code = wayland_scanner::generate_from_xml(&sources)
        .unwrap_or_else(|error| panic!("protocol generation failed: {error}"));
    fs::write(
        PathBuf::from(env::var("OUT_DIR").unwrap()).join("protocol.rs"),
        code,
    )
    .unwrap();
}
