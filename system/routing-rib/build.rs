//! 原生 RIB 服务沿用统一服务链接脚本与启动约定。
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed={manifest}/../native-runtime/service.ld");
    println!("cargo:rustc-link-arg-bins=-T{manifest}/../native-runtime/service.ld");
    println!(
        "cargo:rustc-link-arg-bins=--defsym=SERVICE_BASE={:#x}",
        launch_protocol::routing_rib::BASE
    );
}
