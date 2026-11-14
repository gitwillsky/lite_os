"""构建 ESnet iperf3，并通过产品 userland 清单发布。"""
from __future__ import annotations

import shlex
import shutil
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

from build_cache import (
    build_environment, build_jobs_override, cache_lock, fingerprint, make_command, manifest_matches,
    publish_directory, sha256, temporary_directory, write_manifest,
)
from build_target import target_from_environment
from local_inputs import INPUTS
from toolchain.musl import MuslCachePaths, find_stripper, run
from userland import UserlandArtifact
from userland.rust import verify_elf

ROOT = Path(__file__).resolve().parents[2]
VERSION = "3.22"
ARCHIVE = f"iperf-{VERSION}.tar.gz"
URL = f"https://github.com/esnet/iperf/releases/download/{VERSION}/{ARCHIVE}"
SHA256 = "1c0d0fb02c52626111d6e132db80edfbf27bbaff8bd9245df2a371dcb0b35a92"


def _source_archive() -> Path:
    directory = INPUTS / "iperf"
    source = directory / ARCHIVE
    with cache_lock(directory / ".lock"):
        if source.is_file() and sha256(source) == SHA256:
            return source
        incoming = source.with_suffix(".download")
        try:
            with urllib.request.urlopen(URL, timeout=60) as response, incoming.open("wb") as output:
                shutil.copyfileobj(response, output)
            if sha256(incoming) != SHA256:
                raise RuntimeError(f"incorrect SHA-256 for {ARCHIVE}")
            incoming.replace(source)
        finally:
            incoming.unlink(missing_ok=True)
    return source


def _build(musl: MuslCachePaths) -> Path:
    """构建或 ELF 校验失败时不发布缓存。"""
    target = target_from_environment()
    linker_driver = ROOT / "scripts/musl_clang.py"
    payload = {
        "arch": target.arch, "source": SHA256, "sysroot": musl.sysroot_fingerprint,
        "recipe": sha256(Path(__file__)), "linker_driver": sha256(linker_driver),
    }
    cache = ROOT / "target" / "iperf-runtime" / target.arch
    final = cache / fingerprint(payload)
    with cache_lock(cache / ".lock"):
        if not manifest_matches(final, payload, ("iperf3", "LICENSE")):
            archive = _source_archive()
            temporary = temporary_directory(cache, "iperf")
            try:
                with tempfile.TemporaryDirectory(prefix="liteos-iperf-") as workspace:
                    work = Path(workspace)
                    with tarfile.open(archive, "r:gz") as package:
                        package.extractall(work, filter="data")
                    source = work / f"iperf-{VERSION}"
                    env = build_environment()
                    env.pop("LITEOS_RUST_PROVIDES_COMPILER_BUILTINS", None)
                    env.update({
                        "CC": shlex.join((sys.executable, str(linker_driver))),
                        "AR": str(musl.archiver), "RANLIB": str(musl.ranlib),
                        "CFLAGS": "-O2 -fPIE -ffunction-sections -fdata-sections",
                        "CPPFLAGS": "",
                        "LDFLAGS": "-pie -Wl,--gc-sections,-z,relro,-z,now,-z,noexecstack",
                        "LITEOS_MUSL_CLANG": str(musl.compiler),
                        "LITEOS_MUSL_LLD": str(musl.linker),
                        "LITEOS_MUSL_SYSROOT": str(musl.install),
                        "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime),
                    })
                    build = run(["sh", str(source / "config/config.guess")], source, env).strip()
                    run(
                        ["sh", "configure", f"--build={build}", f"--host={target.linux_triple}",
                         "--disable-shared", "--without-openssl", "--without-sctp",
                         "--without-ldconfig", "--disable-maintainer-mode"],
                        source, env,
                    )
                    run([*make_command(build_jobs_override()), "-C", "src", "iperf3"], source, env)
                    binary = temporary / "iperf3"
                    shutil.copy2(source / "src/iperf3", binary)
                    run([str(find_stripper(musl.compiler)), "--strip-unneeded", str(binary)], source, env)
                    verify_elf(binary, musl.compiler, identity="iperf3")
                    shutil.copy2(source / "LICENSE", temporary / "LICENSE")
                write_manifest(temporary, payload)
                publish_directory(temporary, final)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)
    return final


def iperf_userland(musl: MuslCachePaths) -> tuple[UserlandArtifact, ...]:
    """返回标准命令与上游许可证的安装位置。"""
    final = _build(musl)
    return (
        UserlandArtifact(final / "iperf3", "/usr/bin/iperf3", 0o755),
        UserlandArtifact(final / "LICENSE", "/usr/share/licenses/iperf3/LICENSE"),
    )
