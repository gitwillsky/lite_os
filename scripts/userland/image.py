"""产品 rootfs 装配与内容寻址发布。"""
from __future__ import annotations

import shutil
import sys
import tempfile
from apk_cache import cached_apk_bootstrap
from apk_rootfs import assemble_apk_rootfs
from build_cache import (
    build_environment,
    cache_lock,
    fingerprint,
    generation_directory,
    manifest_matches,
    prune_rootfs_images,
    publish_directory,
    publish_generation,
    sha256,
    temporary_directory,
    write_manifest,
)
from build_target import target_from_environment
from classic_assets import boot_scene
from ext4_image import debugfs_path, find_debugfs, find_mke2fs, verify_root_owned_files
from openssl_cache import OpenSslPaths
from pathlib import Path
from userland.artifacts import base_userland
from userland.busybox import BUSYBOX_LINKS, FORBIDDEN_BOOT_MARKERS
from toolchain.musl import MuslCachePaths, run

ROOT = Path(__file__).resolve().parents[2]
TARGET = target_from_environment()
WORK = ROOT / "target" / "busybox-runtime" / TARGET.arch

# OWNER: userland.image 唯一发布 content-addressed rootfs image cache。
# PROOF: 单一文件锁串行化 writer；create_image 完成全部 ownership 断言后才写 manifest，并把
# 完整 fingerprint directory 原子发布为不可变 generation。
# FAILURE: 缺少该 cache 会让 ext4 创建时间在每次 build 改写 fs.img，即使执行输入未变也会
# 使全部下游 APK install/runtime gate 失效。
ROOTFS_RECIPE_VERSION = 17

# APK assembly 同时保留已安装文件、压缩包及 APK 的原子替换临时文件。
# 容量按真实发布输入推导；固定最小值只覆盖小型 rootfs，不能限制新增运行库。
ROOTFS_MIN_SIZE_MIB = 512

STRESS_LINKS = ("cputest", "memtest", "cachetest")


def musl_development_commands(install: Path) -> list[str]:
    """Returns debugfs commands installing the musl headers, startup objects and static archives.

    `liteos-base` provides `musl-dev` with these files, so Alpine packages that depend on `musl-dev`
    (rust, gcc) resolve against the musl actually shipped here instead of pulling Alpine's own musl,
    which would conflict with the file and `so:libc.musl` ownership of `liteos-base`.
    """
    commands: list[str] = []
    include = install / "usr/include"
    directories = sorted({Path("/usr/include"), *(
        Path("/usr/include") / path.relative_to(include)
        for path in include.rglob("*")
        if path.is_dir()
    )})
    commands.extend(f"mkdir {directory}" for directory in directories)
    files = [
        (path, Path("/usr/include") / path.relative_to(include))
        for path in sorted(include.rglob("*"))
        if path.is_file()
    ]
    library = install / "usr/lib"
    files.extend(
        (path, Path("/usr/lib") / path.name)
        for path in sorted(library.iterdir())
        if path.suffix in (".a", ".o")
    )
    for source, destination in files:
        commands.append(f"write {source} {destination}")
        commands.append(f"set_inode_field {destination} mode 0100644")
    return commands


def build_stress_tools(musl: MuslCachePaths) -> Path:
    """构建 rootfs 单一 CPU/memory/page-cache 诊断程序。"""
    source = ROOT / "user/diagnostics/liteos-stress.c"
    payload = {
        "kind": "liteos-stress-tools",
        "recipe_version": 1,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "source_sha256": sha256(source),
    }
    entry = WORK / "stress-tools" / fingerprint(payload)
    if manifest_matches(entry, payload, ("liteos-stress",)):
        return entry / "liteos-stress"
    generation = generation_directory(WORK / "stress-tool-generations", fingerprint(payload))
    env = build_environment()
    env.update({
        "LITEOS_MUSL_CLANG": str(musl.compiler),
        "LITEOS_MUSL_LLD": str(musl.linker),
        "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime),
        "LITEOS_MUSL_SYSROOT": str(musl.install),
    })
    published = False
    try:
        run(
            [
                sys.executable,
                str(ROOT / "scripts/musl_clang.py"),
                str(source),
                "-std=c11",
                "-D_GNU_SOURCE",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fPIE",
                "-pie",
                "-pthread",
                "-o",
                str(generation / "liteos-stress"),
            ],
            ROOT,
            env,
        )
        write_manifest(generation, payload)
        publish_generation(generation, entry)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return entry / "liteos-stress"


def create_image(
    binary: Path,
    musl: MuslCachePaths,
    openssl: OpenSslPaths,
    image: Path,
) -> Path:
    """构造 BusyBox、唯一 musl runtime、标准 loader symlink 与固定 inittab。"""
    userland = base_userland(musl)
    stress_tools = build_stress_tools(musl)
    payload_bytes = sum(artifact.source.stat().st_size for artifact in userland)
    payload_bytes += sum(path.stat().st_size for path in (binary, openssl.binary, stress_tools))
    payload_bytes += sum(path.stat().st_size for path in (musl.install / "usr").rglob("*") if path.is_file())
    # 1. 现有树与压缩包同时存在；2. apk 在原文件旁创建替换文件；3. 另留元数据和块对齐空间。
    # 三份未压缩输入是保守上界，64 MiB 对齐保持 sparse image，避免依赖压缩率而在安装中 ENOSPC。
    size_mib = max(ROOTFS_MIN_SIZE_MIB, ((3 * payload_bytes + 64 * 1024 * 1024 + (64 * 1024 * 1024 - 1)) // (64 * 1024 * 1024)) * 64)
    run(
        [
            sys.executable,
            "create_fs.py",
            "create",
            "--file",
            str(image),
            "--size",
            str(size_mib),
            "--init",
            str(binary),
        ],
        ROOT,
    )
    bootstrap = cached_apk_bootstrap()
    commands = [
        "mkdir /etc",
        "mkdir /etc/init.d",
        "mkdir /etc/ssl",
        "mkdir /lib",
        "mkdir /run",
        "mkdir /root",
        "set_inode_field /root mode 040700",
        "mkdir /root/Documents",
        "mkdir /root/Downloads",
        "mkdir /root/Music",
        "mkdir /root/Pictures",
        "mkdir /root/Videos",
        "mkdir /tmp",
        "set_inode_field /tmp mode 041777",
        "mkdir /usr",
        "mkdir /usr/bin",
        "mkdir /usr/lib",
        "mkdir /usr/lib/liteos",
        "mkdir /usr/share",
        "mkdir /usr/share/liteos",
        "mkdir /usr/share/liteos/fonts",
        "mkdir /usr/share/liteos/ui",
        "mkdir /usr/share/liteos/sounds",
        "mkdir /usr/share/applications",
        "mkdir /home",
        "mkdir /usr/share/udhcpc",
        "mkdir /etc/network",
        "mkdir /var",
        "mkdir /var/cache",
        "mkdir /var/empty",
        "mkdir /var/lib",
        "mkdir /var/tmp",
        "set_inode_field /var/tmp mode 041777",
        f"write {ROOT / 'user' / 'base' / 'passwd'} /etc/passwd",
        f"write {ROOT / 'user' / 'base' / 'group'} /etc/group",
        f"write {ROOT / 'user' / 'base' / 'shadow'} /etc/shadow",
        "set_inode_field /etc/shadow mode 0100600",
        # 默认时区（UTC+8）；之后由控制面板改写，同步器只在缺失时补上。
        f"write {ROOT / 'user' / 'base' / 'localtime'} /etc/localtime",
        # 默认网络配置（eth0 DHCP）；之后由控制面板改写，同步器只在缺失时补上。
        f"write {ROOT / 'user' / 'base' / 'interfaces'} /etc/network/interfaces",
        f"write {openssl.binary} /bin/openssl",
        "set_inode_field /bin/openssl mode 0100755",
        f"write {musl.install / 'usr/lib/libc.so'} /usr/lib/libc.so",
        "set_inode_field /usr/lib/libc.so mode 0100755",
        f"write {stress_tools} /bin/liteos-stress",
        "set_inode_field /bin/liteos-stress mode 0100755",
        "ln /bin/liteos-stress /bin/cputest",
        "ln /bin/liteos-stress /bin/memtest",
        "ln /bin/liteos-stress /bin/cachetest",
        f"set_inode_field /bin/liteos-stress links_count {len(STRESS_LINKS) + 1}",
        f"symlink {TARGET.musl_loader} /usr/lib/libc.so",
        *musl_development_commands(musl.install),
    ]
    # 安装清单同时决定目录树：SDK/Mesa 等新增嵌套路径不能依赖另一份手写 mkdir 列表。
    existing_directories = {command.removeprefix("mkdir ") for command in commands if command.startswith("mkdir ")}
    artifact_directories = {str(parent) for artifact in userland for parent in Path(artifact.destination).parents if str(parent) != "/"}
    commands.extend(f"mkdir {debugfs_path(folder)}" for folder in sorted(artifact_directories - existing_directories, key=lambda path: (path.count("/"), path)))
    # 启动配置与共享开机素材只有 base_userland 一份安装清单。
    for artifact in userland:
        commands.append(f"write {debugfs_path(artifact.source)} {debugfs_path(artifact.destination)}")
        commands.append(
            f"set_inode_field {debugfs_path(artifact.destination)} mode 0{0o100000 | artifact.mode:o}"
        )
    commands.extend(f"ln /bin/init /bin/{applet}" for applet in BUSYBOX_LINKS)
    # npm/pnpm use `#!/usr/bin/env node`; without the standard env location apk installs succeed
    # but every JavaScript package-manager entry fails exec with ENOENT.
    commands.append("ln /bin/init /usr/bin/env")
    commands.append(f"set_inode_field /bin/init links_count {len(BUSYBOX_LINKS) + 2}")
    script_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile("w", delete=False) as script:
            script.write("\n".join(commands) + "\n")
            script_path = Path(script.name)
        run([str(find_debugfs()), "-w", "-f", str(script_path), str(image)], ROOT)
    finally:
        if script_path is not None:
            script_path.unlink(missing_ok=True)
    with tempfile.TemporaryDirectory(prefix="liteos-apk-rootfs-") as workspace:
        assemble_apk_rootfs(
            image,
            find_debugfs(),
            Path(workspace),
            BUSYBOX_LINKS,
            STRESS_LINKS,
            FORBIDDEN_BOOT_MARKERS,
            (),
        )
    listing = run([str(find_debugfs()), "-R", "ls -l /bin", str(image)], ROOT)
    entries: dict[str, int] = {}
    for line in listing.splitlines():
        fields = line.split()
        if len(fields) >= 9 and fields[0].isdigit():
            entries[fields[-1]] = int(fields[0])
    expected = {"init", *BUSYBOX_LINKS}
    missing = sorted(expected - entries.keys())
    if missing:
        raise RuntimeError(f"BusyBox rootfs lacks applets: {', '.join(missing)}")
    if len({entries[name] for name in expected}) != 1:
        raise RuntimeError("BusyBox applets must be hardlinks to one inode")
    metadata = run([str(find_debugfs()), "-R", "stat /bin/init", str(image)], ROOT)
    if f"Links: {len(expected) + 1}" not in metadata:
        raise RuntimeError("BusyBox inode link count does not match rootfs applets")
    standard_env = run(
        [str(find_debugfs()), "-R", "stat /usr/bin/env", str(image)], ROOT
    )
    if f"Inode: {entries['init']}" not in standard_env or "Mode:  0755" not in standard_env:
        raise RuntimeError("BusyBox rootfs lacks the standard /usr/bin/env hardlink")
    stress_commands = {"liteos-stress", *STRESS_LINKS}
    missing_stress = sorted(stress_commands - entries.keys())
    if missing_stress:
        raise RuntimeError(f"rootfs lacks stress commands: {', '.join(missing_stress)}")
    if len({entries[name] for name in stress_commands}) != 1:
        raise RuntimeError("stress commands must be hardlinks to one inode")
    stress_metadata = run(
        [str(find_debugfs()), "-R", "stat /bin/liteos-stress", str(image)], ROOT
    )
    if "Links: 4" not in stress_metadata:
        raise RuntimeError("stress command inode link count does not match multicall names")
    temporary_directory_metadata = run(
        [str(find_debugfs()), "-R", "stat /tmp", str(image)], ROOT
    )
    if "Mode:  01777" not in temporary_directory_metadata:
        raise RuntimeError("BusyBox rootfs /tmp must have mode 01777")
    passwd = run([str(find_debugfs()), "-R", "cat /etc/passwd", str(image)], ROOT)
    group = run([str(find_debugfs()), "-R", "cat /etc/group", str(image)], ROOT)
    if "root:x:0:0:root:/root:/bin/sh" not in passwd or "root:x:0:" not in group:
        raise RuntimeError("BusyBox rootfs lacks the canonical root identity records")
    for fixture in ("dynamic-smoke", "liteos-script", "rust-std-smoke"):
        if fixture in entries:
            raise RuntimeError(f"product rootfs contains verification fixture: {fixture}")
    product_libraries = run(
        [str(find_debugfs()), "-R", "ls -l /usr/lib", str(image)], ROOT
    )
    if "libliteos-smoke.so" in product_libraries:
        raise RuntimeError("product rootfs contains verification fixture: libliteos-smoke.so")
    loader = run(
        [str(find_debugfs()), "-R", f"stat {TARGET.musl_loader}", str(image)],
        ROOT,
    )
    if "Type: symlink" not in loader or "Size: 16" not in loader:
        raise RuntimeError("BusyBox rootfs lacks the standard musl loader symlink")
    network_service = run(
        [str(find_debugfs()), "-R", "stat /etc/init.d/network-service", str(image)], ROOT
    )
    if "Type: regular" not in network_service or "Mode:  0755" not in network_service:
        raise RuntimeError("BusyBox rootfs lacks the supervised network service")
    openssl_binary = run([str(find_debugfs()), "-R", "stat /bin/openssl", str(image)], ROOT)
    if "Type: regular" not in openssl_binary or "Mode:  0755" not in openssl_binary:
        raise RuntimeError("BusyBox rootfs lacks the verified HTTPS helper")
    with tempfile.TemporaryDirectory(prefix="liteos-base-readback-") as directory:
        for index, artifact in enumerate(userland):
            observed = Path(directory) / str(index)
            run([str(find_debugfs()), "-R", f"dump {debugfs_path(artifact.destination)} {observed}", str(image)], ROOT)
            if not observed.is_file() or sha256(observed) != sha256(artifact.source):
                raise RuntimeError(f"BusyBox rootfs system file differs: {artifact.destination}")
    verify_root_owned_files(image, tuple((artifact.destination, artifact.mode) for artifact in userland))
    ca_bundle = run([str(find_debugfs()), "-R", "stat /etc/ssl/cert.pem", str(image)], ROOT)
    ca_store = run(
        [str(find_debugfs()), "-R", "stat /etc/ssl/certs/ca-certificates.crt", str(image)],
        ROOT,
    )
    if "Type: symlink" not in ca_bundle or "Type: regular" not in ca_store:
        raise RuntimeError("BusyBox rootfs lacks the package-owned Mozilla CA trust bundle")
    return image


def target_runtime_artifacts() -> tuple[Path, ...]:
    """返回 rootfs 与 runtime gate 必须绑定的目标 release 产物。"""
    artifacts = [ROOT / f"target/{TARGET.kernel_triple}/release/kernel"]
    if TARGET.requires_bootloader:
        artifacts.append(
            ROOT / f"bootloader/target/{TARGET.kernel_triple}/release/bootloader"
        )
    return tuple(artifacts)


def create_published_image(
    binary: Path,
    musl: MuslCachePaths,
    openssl: OpenSslPaths,
    output: Path,
) -> Path:
    """从内容寻址缓存原子发布一份可写 rootfs。

    Args:
        binary: 已校验的 BusyBox ELF。
        musl: 已校验的 musl sysroot 与工具链 identity。
        openssl: 已校验的 target OpenSSL 产物。
        output: 调用方独占的可写镜像路径。

    Returns:
        已原子替换为完整缓存镜像的 output。

    Raises:
        RuntimeError: 构建工具、APK bootstrap 或 rootfs assembly 失败。
        OSError: cache publication 或 output copy 失败。
    """
    userland = base_userland(musl)
    stress_tools = build_stress_tools(musl)
    bootstrap = cached_apk_bootstrap()
    host_openssl = shutil.which("openssl")
    if host_openssl is None:
        raise RuntimeError("host OpenSSL is required to sign the rootfs package")
    alpine_keys = tuple(
        sorted(path for path in bootstrap.alpine_keys.iterdir() if path.is_file())
    )
    inputs = (
        *target_runtime_artifacts(),
        binary,
        musl.install / "usr/lib/libc.so",
        *(artifact.source for artifact in userland),
        stress_tools,
        openssl.binary,
        bootstrap.apk_static,
        bootstrap.ca_certificates_bundle,
        bootstrap.private_key,
        bootstrap.public_key,
        *alpine_keys,
        ROOT / "user/base/passwd",
        ROOT / "user/base/shadow",
        ROOT / "user/base/group",
        ROOT / "user/base/inittab",
        ROOT / "user/base/profile",
        ROOT / "user/base/localtime",
        ROOT / "user/diagnostics/liteos-stress.c",
        boot_scene(),
        ROOT / "user/base/network-service",
        ROOT / "user/base/shutdown",
        ROOT / "user/base/udhcpc.script",
        ROOT / "user/base/interfaces",
        ROOT / "create_fs.py",
        *(ROOT / "scripts/userland" / name for name in ("image.py", "artifacts.py", "busybox.py", "rust.py")),
        ROOT / "scripts/apk_cache.py",
        ROOT / "scripts/apk_package.py",
        ROOT / "scripts/apk_rootfs.py",
        ROOT / "scripts/ext4_image.py",
        ROOT / "scripts/qemu_gate.py",
        find_mke2fs(),
        find_debugfs(),
        Path(host_openssl),
    )
    # rootfs 是纯构建产物，缓存身份只取决于输入 bytes 与 recipe。APK 数据库虽然要在
    # target guest 中生成，但 host QEMU 可执行文件的版本不改变最终镜像语义，不能进入缓存键。
    payload = {
        "kind": "busybox-rootfs-image",
        "recipe_version": ROOTFS_RECIPE_VERSION,
        "arch": TARGET.arch,
        "inputs": {str(path): sha256(path) for path in inputs},
    }
    identity = fingerprint(payload)
    cache = WORK / "rootfs-images" / identity
    with cache_lock(WORK / ".rootfs.lock"):
        if manifest_matches(cache, payload, ("fs.img",)):
            print(f"BusyBox rootfs cache hit: {identity[:12]}")
        else:
            temporary = temporary_directory(WORK / "rootfs-images", "rootfs")
            try:
                create_image(binary, musl, openssl, temporary / "fs.img")
                write_manifest(temporary, payload)
                publish_directory(temporary, cache)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)

        # OWNER/PROOF: 同一 rootfs 锁覆盖发布、读取与淘汰，consumer 只拿独立副本。
        # FAILURE: 若复制在锁外，另一个 builder/清理进程可在 copyfile 之前删除源镜像。
        output.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".liteos-rootfs-", dir=output.parent) as directory:
            temporary = Path(directory) / "fs.img"
            shutil.copyfile(cache / "fs.img", temporary)
            temporary.replace(output)
        prune_rootfs_images(WORK, current=cache)
    return output
