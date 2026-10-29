"""系统 Rust 程序及共享库的唯一构建与发布。"""
from __future__ import annotations

import shutil
from build_cache import (
    build_environment,
    cache_lock,
    fingerprint,
    generation_directory,
    manifest_matches,
    publish_generation,
    sha256,
    write_manifest,
)
from build_target import target_from_environment
from dataclasses import dataclass
from pathlib import Path
from toolchain.musl import MuslCachePaths, run, ELF_MACHINE, RUST_USER_TARGET

ROOT = Path(__file__).resolve().parents[2]
TARGET = target_from_environment()
WORK = ROOT / "target" / "busybox-runtime" / TARGET.arch

# 3：动态世界改为一次 Cargo 调用整体构建，静态世界不再以系统库包为主包；旧 cache 里可能留有
# 分批构建产出的同名不同身份 dylib，Cargo 视其为新鲜，必须换目录作废。
RUST_USER_CARGO_CACHE_RECIPE_VERSION = 3

RUST_USER_BUILD_STD = "std,panic_abort;llvm-libunwind"

# 系统共享库（对应 Windows 2000 的 DLL）与动态链接它们的程序；探针、session-launch 等其余程序整体静态。
# 运行时库承载 std，其余库与程序都链接它，所以全系统只有一份 std。Rust 没有稳定 ABI，库与程序必须由同一份
# 工具链、同一组参数、同一个 Cargo target 一次构建，并一起安装到 `/usr/lib/liteos`。
# 元素是 `(Cargo 包名, 库名)`，库文件名是 `lib<库名>.so`；顺序即依赖顺序。
RUNTIME_LIBRARY = "liteos_runtime"

UI_RUNTIME = ("classic-ui-runtime", "liteos_ui")

SYSTEM_LIBRARIES = (
    ("liteos-runtime", RUNTIME_LIBRARY), ("linux-uapi", "linux_uapi"),
    ("canvas", "canvas"), ("classic-theme", "classic_theme"), ("typeface", "typeface"),
    ("classic-ui", "classic_ui"), ("wayland-wire", "wayland_wire"),
    ("keyboard", "keyboard"), ("calendar", "calendar"), ("desktop-entry", "desktop_entry"),
    ("desktop-settings", "desktop_settings"),
    ("sound", "sound"), ("sndvol", "sndvol"),
    UI_RUNTIME,
)

SYSTEM_LIBRARY_DIRECTORY = "/usr/lib/liteos"

# 动态世界的程序：链接系统 Rust dylib 的产品程序与门禁探针。Rust dylib 没有稳定 ABI，
# 它们必须与系统库出自同一次 Cargo 调用并整体发布，门禁直接取用这次的探针，不另行构建。
DYNAMIC_PROGRAMS = frozenset({
    ("compositor", "compositor"), ("logon", "logon"), ("desktop", "desktop"),
    ("sound-player", "sndplay"), ("smbfs", "smbfs"), ("linux-uapi", "session-launch"),
    ("compositor", "wayland-probe"), ("compositor", "shell-probe"),
    ("classic-ui", "ui-probe"), ("classic-ui", "session-probe"), ("linux-uapi", "drm-flip-probe"),
})

STABLE_UI_PROGRAMS = frozenset({("file-explorer", "file-explorer"), ("control-panel", "control-panel"), ("notepad", "notepad"), ("device-manager", "devmgmt"), ("volume-control", "sndvol"), ("terminal", "terminal"), ("task-manager", "taskmgr"), ("media-player", "mplay")})

RUST_USER_RUSTFLAGS = (
    "-C link-self-contained=no -C target-feature=-crt-static "
    "-C relocation-model=pic -C panic=abort "
    "-C link-arg=-Wl,--gc-sections,-z,relro,-z,now,-z,noexecstack"
)


def rust_user_cargo_target(
    musl: MuslCachePaths,
    cargo_version: str,
    rustc_version: str,
    driver_sha256: str,
    libunwind_sha256: str,
    dynamic: bool = False,
) -> Path:
    """返回只绑定编译环境、不绑定源码 revision 的持久 Cargo cache。

    Args:
        musl: 当前目标架构的 musl sysroot 身份。
        cargo_version: 实际执行的 Cargo 版本字符串。
        rustc_version: 实际执行的 rustc 版本字符串。
        driver_sha256: 唯一 musl linker driver 的内容摘要。
        libunwind_sha256: 当前静态 libunwind 的内容摘要。

    Returns:
        由完整编译环境身份隔离的 mutable Cargo target 目录。
    """
    compilation_payload = {
        "kind": "rust-user-cargo-target",
        "dynamic": dynamic,
        "recipe_version": RUST_USER_CARGO_CACHE_RECIPE_VERSION,
        "arch": TARGET.arch,
        "rust_target": RUST_USER_TARGET,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": driver_sha256,
        "cargo": cargo_version,
        "rustc": rustc_version,
        "build_std": RUST_USER_BUILD_STD,
        "libunwind_sha256": libunwind_sha256,
        "rustflags": f"{RUST_USER_RUSTFLAGS} -L native=<libunwind> -D warnings",
    }
    return WORK / "rust-user-cargo-targets" / fingerprint(compilation_payload)


@dataclass(frozen=True)
class _RustUserTarget:
    package: str
    name: str
    kind: str
    recipe_version: int
    library: bool = False

    @property
    def dynamic(self) -> bool:
        if self.library:
            return (self.package, self.name) != UI_RUNTIME
        return (self.package, self.name) in DYNAMIC_PROGRAMS

    @property
    def sdk_consumer(self) -> bool:
        return not self.library and (self.package, self.name) in STABLE_UI_PROGRAMS

    @property
    def artifact(self) -> str:
        return f"lib{self.name}.so" if self.library else self.name


def _dynamic_world() -> tuple[_RustUserTarget, ...]:
    """动态世界的全部成员：系统 Rust dylib 与链接它们的全部程序，总是由同一次 Cargo 调用产出。"""
    return (
        *(_RustUserTarget(package, name, f"system-library-{name}", 1, True)
          for package, name in SYSTEM_LIBRARIES if (package, name) != UI_RUNTIME),
        *(_RustUserTarget(package, name, name, 1) for package, name in DYNAMIC_PROGRAMS),
    )


def _llvm_tool(compiler: Path, name: str) -> Path:
    """与目标工具链同前缀的 LLVM 工具；缺失时退回 PATH 或 Homebrew LLVM。"""
    prefixed = Path(f"{str(compiler)[: -len('gcc')]}{name}")
    if prefixed.is_file():
        return prefixed
    return Path(shutil.which(f"llvm-{name}") or f"/opt/homebrew/opt/llvm/bin/llvm-{name}")


def verify_dynamic_world(compiler: Path, release: Path, artifacts: tuple[str, ...]) -> None:
    """一次构建产出的动态世界必须符号自洽：每个 Rust 符号引用都由其 NEEDED 闭包里的系统库定义。

    Rust dylib 没有稳定 ABI，crate 身份（StableCrateId）随特性解析、主包身份等构建条件变化；
    同名 .so 若来自另一份身份，程序要到加载时才报 symbol not found。这里在发布前拦下。
    """
    readelf, nm = _llvm_tool(compiler, "readelf"), _llvm_tool(compiler, "nm")

    def needed(path: Path) -> set[str]:
        output = run([str(readelf), "--dynamic", "--wide", str(path)], ROOT)
        return {line.split("Shared library: [", 1)[1].split("]", 1)[0]
                for line in output.splitlines() if "Shared library: [" in line}

    def symbols(path: Path, kind: str) -> set[str]:
        output = run([str(nm), "-D", f"--{kind}-only", "--format=just-symbols", str(path)], ROOT)
        return {symbol for symbol in output.split() if symbol.startswith(("_R", "_ZN"))}

    libraries = {name: release / name for name in artifacts if name.endswith(".so")}
    defined = {name: symbols(path, "defined") for name, path in libraries.items()}
    dependencies = {name: needed(release / name) for name in artifacts}
    for artifact in artifacts:
        closure, pending = set(), [artifact]
        while pending:
            for library in dependencies.get(pending.pop(), set()) & libraries.keys():
                if library not in closure:
                    closure.add(library)
                    pending.append(library)
        provided = set().union(*(defined[library] for library in closure))
        missing = symbols(release / artifact, "undefined") - provided
        if missing:
            raise RuntimeError(f"{artifact} references Rust symbols its system libraries do not define: {sorted(missing)[:3]}")


def _build_rust_user_targets(
    musl: MuslCachePaths, requested: tuple[_RustUserTarget, ...]
) -> dict[_RustUserTarget, Path]:
    """Cargo 拥有依赖新鲜度；本入口校验并按最终 ELF 内容发布不可变产物。"""
    cargo = shutil.which("cargo")
    rustc = shutil.which("rustc")
    if cargo is None or rustc is None:
        raise RuntimeError("nightly Cargo and rustc are required for Rust userspace")
    available_targets = set(run([rustc, "--print", "target-list"], ROOT).splitlines())
    if RUST_USER_TARGET not in available_targets:
        raise RuntimeError(
            f"rustc does not provide required {TARGET.arch} userspace target "
            f"{RUST_USER_TARGET}; refusing to reuse another architecture"
        )
    cargo_version = run([cargo, "--version"], ROOT).strip()
    rustc_version = run([rustc, "--version"], ROOT).strip()
    ui_target = _RustUserTarget(*UI_RUNTIME, "system-library-liteos_ui", 1, True)
    ui_dependencies = (ui_target,) if any(target.sdk_consumer for target in requested) else ()
    targets = tuple(dict.fromkeys((*ui_dependencies, *requested)))
    for target in targets:
        if target.library and (target.package, target.name) not in SYSTEM_LIBRARIES:
            raise RuntimeError(f"{target.package} is not a registered system library")
        if not target.library and not target.dynamic and not target.sdk_consumer:
            raise RuntimeError(f"{target.package}/{target.name} is neither a dynamic-world nor an SDK program")
    from toolchain.unwind import build_libunwind

    # OWNER/PROOF: 同一 build lock 覆盖 Cargo 的可变目录、UI 链接输入与不可变发布。
    # FAILURE: 只依赖 Cargo 自己的锁，会在退出 Cargo 到复制 ELF 之间被另一轮构建替换文件。
    with cache_lock(WORK / ".build.lock"):
        libunwind = build_libunwind(musl, rustc)
        driver_sha256 = sha256(ROOT / "scripts/musl_clang.py")
        unwind_sha256 = sha256(libunwind)
        cargo_targets = {
            dynamic: rust_user_cargo_target(musl, cargo_version, rustc_version, driver_sha256, unwind_sha256, dynamic)
            for dynamic in (False, True)
        }
        # 1. 动态世界任一成员被请求时整体构建一次：系统 dylib 与全部链接它们的程序（含门禁探针）
        #    出自同一次 Cargo 调用、同一特性解析，整体发布；分批构建会得到同名不同 crate 身份的 .so。
        # 2. 静态世界（UI 运行库、SDK 应用）独立目录，先完成 UI 运行库，SDK 应用才可链接它。
        world = _dynamic_world()
        groups = (
            tuple(target for target in targets if target == ui_target),
            world if any(target.dynamic for target in targets) else (),
            *((target,) for target in targets if target.sdk_consumer),
        )
        published = {}
        for group in groups:
            if not group:
                continue
            first = group[0]
            # Cargo 只给非主包的 dylib 自动加 -C prefer-dynamic，而单元哈希不含主包身份；
            # 静态世界若选中系统库包，产出的 .so 会静态嵌入 liteos_runtime 并被后续复用。
            system_packages = {package for package, name in SYSTEM_LIBRARIES if (package, name) != UI_RUNTIME}
            if not first.dynamic and any(target.package in system_packages for target in group):
                raise RuntimeError(f"static Rust build must not select a system library package: {group}")
            cargo_target = cargo_targets[first.dynamic]
            env = build_environment()
            linker_variable = f"CARGO_TARGET_{RUST_USER_TARGET.upper().replace('-', '_')}_LINKER"
            env.update({
                "LITEOS_MUSL_CLANG": str(musl.compiler),
                "LITEOS_MUSL_LLD": str(musl.linker),
                "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime),
                "LITEOS_MUSL_SYSROOT": str(musl.install),
                "LITEOS_RUST_PROVIDES_COMPILER_BUILTINS": "1",
                "CARGO_INCREMENTAL": "0",
                "CARGO_TARGET_DIR": str(cargo_target),
                linker_variable: str(ROOT / "scripts/musl_clang.py"),
                "RUSTFLAGS": (
                    f"{RUST_USER_RUSTFLAGS} "
                    f"{'-C prefer-dynamic ' if first.dynamic else ''}"
                    f"-L native={libunwind.parent} -D warnings"
                ),
            })
            if first.dynamic:
                # prefer-dynamic 与 LTO 互斥；只改变内部 Rust dylib 的编译组。
                env["CARGO_PROFILE_RELEASE_LTO"] = "off"
            command = [
                cargo, "rustc" if first.sdk_consumer else "build",
                "-Z", "build-std=std,panic_abort",
                "-Z", "build-std-features=llvm-libunwind",
                "--manifest-path", str(ROOT / "user/Cargo.toml"),
                "--target", RUST_USER_TARGET, "--release", "--locked",
            ]
            for package in dict.fromkeys(target.package for target in group):
                command.extend(("-p", package))
            if any(target.library for target in group):
                command.append("--lib")
            for target in group:
                if not target.library:
                    command.extend(("--bin", target.name))
            if first.sdk_consumer:
                # 2. 目录只传给最终 bin 的 rustc，不污染 std/依赖的 RUSTFLAGS；
                # 此可变输入由同一 build lock 保护，发布目录变化不会触发整组重编。
                command.extend(("--", "-L", f"native={cargo_targets[False] / RUST_USER_TARGET / 'release'}"))
            run(command, ROOT, env)
            release = cargo_target / RUST_USER_TARGET / "release"
            if group is world:
                verify_dynamic_world(musl.compiler, release, tuple(target.artifact for target in world))
            # 3. Cargo 已检查真实依赖（含 XML、include! 和 build.rs）；只按最终 bytes 发布被请求的产物。
            for target in (tuple(target for target in targets if target.dynamic) if group is world else group):
                built = cargo_target / RUST_USER_TARGET / "release" / target.artifact
                if not built.is_file():
                    raise RuntimeError(f"Cargo did not produce {target.artifact}")
                payload = {
                    "kind": target.kind,
                    "recipe_version": target.recipe_version,
                    "arch": TARGET.arch,
                    "compilation": cargo_target.name,
                    "artifact_sha256": sha256(built),
                }
                entry = WORK / "rust-user-programs" / fingerprint(payload)
                required_libraries = ("libc.so", "libliteos_ui.so.1") if target.sdk_consumer else (
                    (f"lib{RUNTIME_LIBRARY}.so",)
                    if target.dynamic and target.name != RUNTIME_LIBRARY else ("libc.so",)
                )
                if manifest_matches(entry, payload, (target.artifact,)) and sha256(entry / target.artifact) == payload["artifact_sha256"]:
                    verify_user_artifact(entry / target.artifact, musl.compiler, required_libraries, target.artifact, target.library)
                else:
                    generation = generation_directory(WORK / "rust-user-program-generations", fingerprint(payload))
                    try:
                        shutil.copy2(built, generation / target.artifact)
                        verify_user_artifact(generation / target.artifact, musl.compiler, required_libraries, target.artifact, target.library)
                        write_manifest(generation, payload)
                        publish_generation(generation, entry)
                    except BaseException:
                        shutil.rmtree(generation, ignore_errors=True)
                        raise
                published[target] = entry / target.artifact
        return published


def build_rust_user_program(
    musl: MuslCachePaths,
    crate_name: str,
    binary_name: str,
    kind: str,
    recipe_version: int,
    *,
    library: bool = False,
) -> Path:
    """经统一批量构建入口生成一个 gate 探针或系统产物。

    Args:
        musl: 当前目标的 musl sysroot 与工具链。
        crate_name: Cargo package 名。
        binary_name: bin 或 library 名。
        kind: 不可变发布清单的产物类别。
        recipe_version: 调用方发布配方版本。
        library: 是否构建已登记的系统共享库。

    Returns:
        经过 ELF 校验的不可变发布文件。

    Raises:
        RuntimeError: 工具链、目标、Cargo 构建或 ELF 校验失败。
    """
    target = _RustUserTarget(crate_name, binary_name, kind, recipe_version, library)
    return _build_rust_user_targets(musl, (target,))[target]


def verify_user_artifact(
    path: Path, compiler: Path, required_libraries: tuple[str, ...], identity: str, library: bool
) -> None:
    """程序按动态 PIE 校验；系统库按共享对象校验（没有 interpreter，也不是 PIE 入口）。"""
    if not library:
        verify_elf(path, compiler, required_libraries, identity)
        return
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        readelf = Path(shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf")
    output = run([str(readelf), "--file-header", "--program-headers", "--dynamic", "--wide", str(path)], ROOT)
    for marker in ("ELF64", ELF_MACHINE, "DYN (", "GNU_RELRO"):
        if marker not in output:
            raise RuntimeError(f"{identity} shared library lacks {marker!r}")
    needed = {
        line.split("Shared library: [", 1)[1].split("]", 1)[0]
        for line in output.splitlines()
        if "Shared library: [" in line
    }
    allowed = set(required_libraries)
    if identity != f"lib{UI_RUNTIME[1]}.so":
        allowed |= {"libc.so"} | {f"lib{name}.so" for _, name in SYSTEM_LIBRARIES}
    if not set(required_libraries) <= needed or not needed <= allowed:
        raise RuntimeError(f"{identity} dependencies differ: expected {sorted(required_libraries)}, got {sorted(needed)}")
    if "TEXTREL" in output or "Requesting program interpreter" in output:
        raise RuntimeError(f"{identity} must be a position-independent shared object")
    for line in output.splitlines():
        columns = line.split()
        if len(columns) >= 8 and columns[0] == "GNU_STACK" and "E" in "".join(columns[6:-1]):
            raise RuntimeError(f"{identity} requests an executable stack")


def verify_elf(
    binary: Path,
    compiler: Path,
    required_libraries: tuple[str, ...] = ("libc.so",),
    identity: str = "BusyBox",
) -> None:
    """要求目标架构动态 PIE、标准 musl interpreter、RELRO、W^X 与 NX stack。"""
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        candidate = shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf"
        readelf = Path(candidate)
    if not readelf.is_file():
        raise RuntimeError(f"{TARGET.arch} readelf or llvm-readelf is required")
    output = run(
        [str(readelf), "--file-header", "--program-headers", "--dynamic", "--wide", str(binary)], ROOT
    )
    for marker in ("ELF64", ELF_MACHINE, "DYN ("):
        if marker not in output:
            raise RuntimeError(f"{identity} ELF lacks {marker!r}")
    headers = [line.split() for line in output.splitlines()]
    if (
        output.count("Requesting program interpreter:") != 1
        or TARGET.musl_loader not in output
    ):
        raise RuntimeError(
            f"{identity} must use the standard {TARGET.arch} musl interpreter"
        )
    for marker in ("DYNAMIC", "GNU_RELRO", "NOW PIE"):
        if marker not in output:
            raise RuntimeError(f"{identity} dynamic ELF lacks {marker!r}")
    needed = {
        line.split("Shared library: [", 1)[1].split("]", 1)[0]
        for line in output.splitlines()
        if "Shared library: [" in line
    }
    # 必需库必须出现；动态程序还可以依赖其他系统共享库，但不能依赖这之外的任何库。
    # SDK 应用只依赖稳定运行库和 musl；不能悄悄重新引入系统 Rust metadata 边界。
    allowed = set(required_libraries)
    if "libliteos_ui.so.1" not in required_libraries:
        allowed |= {"libc.so"} | {f"lib{name}.so" for _, name in SYSTEM_LIBRARIES}
    if not set(required_libraries) <= needed or not needed <= allowed:
        raise RuntimeError(
            f"{identity} ELF dependencies differ: expected {sorted(required_libraries)}, got {sorted(needed)}"
        )
    if "TEXTREL" in output:
        raise RuntimeError(f"{identity} dynamic ELF contains text relocations")
    loads = [columns for columns in headers if columns and columns[0] == "LOAD"]
    if not loads or not any(int(columns[1], 16) == 0 for columns in loads):
        raise RuntimeError(f"{identity} PIE PHDR table is not covered by an offset-zero LOAD")
    for columns in headers:
        if len(columns) < 8 or columns[0] not in {"LOAD", "GNU_STACK"}:
            continue
        flags = "".join(columns[6:-1])
        if columns[0] == "LOAD" and "W" in flags and "E" in flags:
            raise RuntimeError(f"{identity} contains a writable executable LOAD")
        if columns[0] == "GNU_STACK" and "E" in flags:
            raise RuntimeError(f"{identity} requests an executable stack")

