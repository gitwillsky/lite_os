# Portable LLVM Strip Discovery

## Problem

The BusyBox host build adapter resolves Clang, LLVM archive tools, and Rust LLD
portably, but still hard-codes the macOS Homebrew path for `llvm-strip`. On Linux,
the installed `/usr/bin/llvm-strip` is ignored and `make build` fails while
constructing the BusyBox cache fingerprint.

## Design

Add one private helper in `scripts/verify_busybox.py` that locates `llvm-strip`
on `PATH` and falls back to the existing Homebrew path. It returns the resolved
file path or raises a clear `RuntimeError` when neither candidate exists.

Resolve the tool once for each BusyBox cache/build operation. Use that same path
for both the binary fingerprint and BusyBox's `STRIP=` make variable so the cache
identity always describes the tool that actually produced the artifact.

This change stays inside the host-side BusyBox build adapter. It does not change
kernel modules, state ownership, scoped interfaces, dependency direction, ABI,
or error/exit/interrupt cleanup paths.

## Alternatives

- Extending `MuslCachePaths` with a strip tool would widen a shared scoped
  interface for a BusyBox-only need.
- Adding an environment-variable override would create an unnecessary build
  interface and another source of cache identity ambiguity.

## Verification

1. Run `python3 scripts/verify_busybox.py --build-only --image fs.img` and confirm
   the Linux `llvm-strip` path is accepted and the BusyBox/rootfs build passes.
2. Run `make run`, confirm LiteOS reaches its BusyBox userspace, then terminate
   QEMU normally.
3. Run `make verify` as the repository's required final gate.
