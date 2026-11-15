fn main() {
    println!("cargo:rerun-if-env-changed=LITEOS_MUSL_CLANG");
    println!("cargo:rerun-if-env-changed=LITEOS_MUSL_SYSROOT");
    println!("cargo:rerun-if-env-changed=LITEOS_MUSL_AR");
    println!("cargo:rerun-if-changed=src/shared_memory/read_only.c");
    let mut build = cc::Build::new();
    if let (Some(compiler), Some(sysroot)) = (
        std::env::var_os("LITEOS_MUSL_CLANG"),
        std::env::var_os("LITEOS_MUSL_SYSROOT"),
    ) {
        build
            .compiler(compiler)
            .include(std::path::PathBuf::from(sysroot).join("usr/include"));
    }
    if let Some(archiver) = std::env::var_os("LITEOS_MUSL_AR") {
        build.archiver(archiver);
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // The signal handler must be able to inspect TLS without a lazy resolver.
        build.flag("-ftls-model=initial-exec");
    }
    build
        .file("src/shared_memory/read_only.c")
        .compile("liteos_read_only_mapping");
}
