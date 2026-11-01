//! 以系统服务链接脚本、devmgr 基址链接。

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed={manifest}/../native-runtime/service.ld");
    println!("cargo:rustc-link-arg-bins=-T{manifest}/../native-runtime/service.ld");
    println!(
        "cargo:rustc-link-arg-bins=--defsym=SERVICE_BASE={:#x}",
        launch_protocol::devmgr::BASE
    );
}
