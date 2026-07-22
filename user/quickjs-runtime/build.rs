use std::{env, path::PathBuf};

fn main() {
    let vendor = PathBuf::from("vendor/quickjs");
    let sources = [
        "quickjs.c",
        "dtoa.c",
        "libregexp.c",
        "libunicode.c",
        "cutils.c",
    ];
    let mut build = cc::Build::new();
    build
        .include(&vendor)
        .define("_GNU_SOURCE", None)
        .define("CONFIG_VERSION", Some("\"2026-06-04\""))
        .std("gnu11")
        .warnings(false);
    for source in sources {
        let path = vendor.join(source);
        println!("cargo:rerun-if-changed={}", path.display());
        build.file(path);
    }
    println!("cargo:rerun-if-changed=src/shim.c");
    build.file("src/shim.c");
    for header in [
        "quickjs.h",
        "quickjs-atom.h",
        "quickjs-opcode.h",
        "dtoa.h",
        "libregexp.h",
        "libregexp-opcode.h",
        "libunicode.h",
        "libunicode-table.h",
        "cutils.h",
        "list.h",
        "VERSION",
    ] {
        println!("cargo:rerun-if-changed={}", vendor.join(header).display());
    }

    // The rootfs builder exports the repository-owned musl compiler inputs. Using its one
    // wrapper here is required; a host cc fallback would produce target-incompatible objects.
    if env::var_os("LITEOS_MUSL_CLANG").is_some() {
        let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
            .parent()
            .and_then(|path| path.parent())
            .expect("quickjs-runtime must live under user/")
            .to_owned();
        build.compiler(root.join("scripts/musl_clang.py"));
    }
    build.compile("quickjs");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=pthread");
    }
}
