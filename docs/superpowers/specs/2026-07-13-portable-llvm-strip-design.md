# Portable LLVM Strip Discovery

## Problem

The musl host build adapter resolves Clang, LLVM archive tools, and Rust LLD
portably, but the BusyBox and OpenSSL adapters still hard-code the macOS
Homebrew path for `llvm-strip`. On Linux, the installed `/usr/bin/llvm-strip` is
ignored: BusyBox fails while constructing its cache fingerprint, and OpenSSL
fails when stripping its completed binary.

The OpenSSL cache fingerprint also omits the strip tool identity even though the
tool changes the published artifact.

## Design

Add one scoped helper in `scripts/verify_musl.py`, the existing host toolchain
adapter, that locates `llvm-strip` on `PATH` and falls back to the existing
Homebrew path. It returns the absolute discovered path or raises a clear
`RuntimeError` when neither candidate exists. The helper must preserve a
discovered symlink instead of resolving it because LLVM multicall tools select
behavior from `argv[0]`.

The scoped interface has exactly two callers: `scripts/verify_busybox.py` and
`scripts/openssl_cache.py`. Each resolves the tool once per cache/build
operation and uses the same path for execution and fingerprinting. The
fingerprints record the path and SHA-256 of the tool that produced each
artifact.

This change stays inside the host-side build adapters. It expands one scoped
host-toolchain interface with the two callers named above, but does not change
kernel modules, state ownership, dependency direction, ABI, or
error/exit/interrupt cleanup paths.

## Alternatives

- Extending `MuslCachePaths` with a strip tool would mix a transient host build
  tool into the published musl cache paths.
- Adding an environment-variable override would create an unnecessary build
  interface and another source of cache identity ambiguity.
- Duplicating discovery in BusyBox and OpenSSL would create two implementations
  that can drift.

## Verification

1. Run `python3 scripts/verify_busybox.py --build-only --image fs.img` and confirm
   the Linux `llvm-strip` path is accepted and the BusyBox, OpenSSL, and rootfs
   build passes.
2. Run `make run`, confirm LiteOS reaches its BusyBox userspace, then terminate
   QEMU normally.
3. Run `make verify` as the repository's required final gate.
