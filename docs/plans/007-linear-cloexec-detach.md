# Plan 007: Linearize CLOEXEC cleanup and detach descriptor destruction

> **Executor instructions**: Do not run Make or commit. Preserve all unrelated
> worktree changes. The fixed comparison point is `d4e59a8`; Plans 001–006 in
> the live worktree are required input.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Category**: performance/correctness
- **Depends on**: none

## Why this matters

`FileDescriptorTable::take_cloexec` starts at slot zero for every descriptor.
Closing `k` CLOEXEC descriptors in a table of `n` slots is therefore O(n × k)
in the worst case. The method also drops the removed entry while the Process
files lock is held; ordinary close and dup3 replacement do the same. A final
descriptor Drop can traverse every epoll instance and release OFD flock state,
so unrelated fd-table users can be blocked behind global cleanup and lock-order
reasoning crosses the table seam.

## Design

- Deepen fd slot/allocation/dup/flags/snapshot/lifecycle behavior into private
  `fs::file::descriptor_table` rather than letting `file.rs` approach the source
  review threshold.
- Under the files lock, only detach an entry. A transient
  `DetachedFileDescriptor` preserves unique ownership until task orchestration
  consumes it outside the lock; it is not persistent state or a second table.
- Apply the same detach rule to close, dup3 replacement, and CLOEXEC cleanup.
  Process-associated record-lock cleanup remains after descriptor close, as in
  the existing sequence.
- CLOEXEC uses one monotonically increasing transient cursor and a fixed stack
  batch of 32 detached entries. Every fd slot is inspected at most once; each
  batch is fully consumed before reuse. No allocation, cache, bitmap, free-list,
  global, or persistent cursor is added.
- Preserve fd numbers, `FD_CLOEXEC`, OFD sharing, descriptor_refs ordering,
  epoll/flock last-reference cleanup, `EBADF/EMFILE/ENOMEM`, atomic pair
  allocation, dup3 replacement publication, fork cloning, FDSize, procfs order,
  and exit-time whole-table detach.

## Complexity and failure consequences

- Before: worst-case O(n × k) slot visits and k files-lock acquisitions.
- After: O(n) slot visits and at most `ceil(k / 32) + 1` files-lock acquisitions.
- If a detached entry were overwritten before consumption, descriptor_refs and
  epoll/flock cleanup would leak; the batch therefore fail-stops unless all slots
  are empty at reuse.
- If Drop occurred under the files lock, global epoll/flock work could recreate
  a lock inversion; all detach consumers must finish close after guard release.

## Static cases

- no CLOEXEC entries; all entries CLOEXEC; alternating holes/flags;
- CLOEXEC counts 1, 31, 32, 33, and exact multiples of 32;
- sparse final CLOEXEC at the highest allocated slot;
- close invalid/hole/live descriptor;
- dup3 into hole, live target, grown table, old-fd error, target-limit error;
- final vs non-final descriptor_refs across fork/dup;
- fork clone OOM, process exit, procfs snapshot, and record-lock release order.

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

Statically verify that no `FileDescriptor` Drop remains reachable while a live
Process files guard is held, and that the CLOEXEC loop has no restart-from-zero
search or heap collection.

## Done criteria

- [x] CLOEXEC slot visits are O(FDSize), with fixed non-allocating batching.
- [x] close, dup3 replacement, and CLOEXEC Drop execute outside files lock.
- [x] Descriptor state still has one owner and no persistent scan/cache state.
- [x] fd ABI, OOM, OFD, epoll/flock, record-lock, fork/exec/exit semantics remain.
- [x] Deep module/interface docs and architecture baseline are current.
- [x] Static/RV64 gates and independent standards/spec reviews pass.
