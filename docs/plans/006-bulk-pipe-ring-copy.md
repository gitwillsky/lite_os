# Plan 006: Copy Pipe rings in at most two contiguous slices

> **Executor instructions**: Do not run Make or commit. Preserve unrelated worktree
> changes. The fixed comparison point is commit `d4e59a8`; Plan 005 constructor
> changes in the live worktree are required input and must remain intact.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: MED
- **Category**: performance
- **Depends on**: Plan 005

## Why this matters

`Pipe::read` and `Pipe::write` currently copy one byte per loop iteration. Each
byte performs indexed access and mutates the ring cursor or length; write also
computes a dynamic modulo for every byte. A 64 KiB transfer therefore executes
65,536 loop iterations while a byte ring can always be represented by at most
two contiguous slices.

## Scope

Only optimize the private byte movement in `kernel/src/ipc.rs`. Keep the same
64 KiB data capacity, one-byte notification capacity, owner lock, endpoint
lifecycle, `PIPE_BUF` admission rule, stream short-write rule, generation updates,
notifier timing, errno mapping, OOM behavior, and public/scoped interfaces.

Do not add unsafe, a second ring representation, a cached tail, a mode flag, a
lock, allocation, architecture-specific code, or a new wait path.

## Implementation

- Read `min(output.len(), state.length)` bytes as the suffix from `head` followed
  by the optional prefix from zero. Advance `head` with at most one subtraction
  and decrement `length` once.
- Write `min(input.len(), available)` bytes as the suffix from the derived tail
  followed by the optional prefix from zero. Increment `length` once.
- Use safe `copy_from_slice`; a transfer can wrap at most once because its count
  never exceeds ring capacity.
- Preserve the existing zero-length result/generation behavior even though all
  syscall entry points already return before reaching Pipe for zero total length.

## Static edge cases

- contiguous read/write ending before the buffer end;
- contiguous read/write ending exactly at the buffer end;
- wrapped read/write with one byte in the second slice;
- wrapped read/write with both slices non-empty;
- full-to-empty read and empty-to-full write;
- partial stream write and atomic `PIPE_BUF` refusal;
- empty/EOF/full/broken branches;
- one-byte notification token construction and signal/drain paths.

## Verification

Run without Make:

```bash
cargo fmt --all -- --check
cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel
cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings
cargo run --quiet -p architecture-check
(cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm)
git diff --check
```

Inspect RV64 release assembly and confirm the Pipe data path no longer contains a
per-byte ring loop or remainder instruction; bulk copies may lower to `memcpy`.

## Done criteria

- [x] Each successful read/write uses at most two contiguous copies.
- [x] Ring head/length are updated once per operation, with no modulo.
- [x] All listed semantics and edge cases remain unchanged.
- [x] No interface or owner changes are introduced.
- [x] Static/RV64 gates and independent standards/spec reviews pass.
