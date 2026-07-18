# Plan 003: Validate MAP_FIXED backing before removing old mappings

> **Executor instructions**: Follow this file step by step, run every gate, do
> not run Make or commit, and update `plans/README.md` when done. Preserve all
> unrelated working-tree changes.
>
> **Drift check (run first)**:
> `git diff --stat d4e59a8..HEAD -- kernel/src/syscall/memory.rs docs/syscall-support.md`
> Stop if `sys_mmap` no longer matches the current-state excerpt.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `d4e59a8`, 2026-07-15

## Why this matters

LiteOS unmaps a `MAP_FIXED` range before checking anonymous fd/offset rules or
resolving and authorizing the file/device backing. A request that returns
`EINVAL`, `EBADF`, `EACCES`, `ENODEV`, or a backing setup error can therefore
destroy a live mapping. Linux v7.1 performs these input/backing checks before its
replacement stage; LiteOS must match that ordering without inventing a stronger
rollback promise for later mapping-installation failure.

## Current state

- `kernel/src/syscall/memory.rs:102-110` calls `unmap_user_mapping` immediately
  after fixed-address alignment checks.
- Anonymous fd/offset validation is later at lines 112-115.
- File descriptor, access, inode type, DRM constraints, and mapping-facade setup
  are later at lines 123-158; all can return after the old VMA is gone.
- The actual map calls at lines 116-181 remain fallible. This plan does not
  promise restoration after their replacement/installation stage.
- Fixed source: `docs/standards-baseline.md:84` names Linux v7.1 `mm/mmap.c` as
  the VM lifecycle baseline. In that source, `do_mmap` validates file size/type,
  sharing, and access before entering the replacement logic in `mmap_region`.

Vulnerable ordering:

```rust
if fixed {
    task.unmap_user_mapping(address, length)?;
}
if flags & MAP_ANONYMOUS != 0 {
    if fd != -1 || offset != 0 { return -EINVAL; }
}
// fd/access/backing checks follow
```

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Check | `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel` | exit 0 |
| Lint | `cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings` | exit 0 |
| Architecture | `cargo run --quiet -p architecture-check` | exit 0 |
| Release link | `cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm` | exit 0 |
| Diff hygiene | `git diff --check` | no output |

## Scope

**In scope**: `kernel/src/syscall/memory.rs`, `docs/syscall-support.md`,
`plans/README.md`.

**Out of scope**: transactional VMA rollback, new MemorySet APIs, mmap flag/ABI
expansion, resource-limit policy, page-fault behavior, and `MAP_FIXED_NOREPLACE`.

## Git workflow

Stay on the current branch. Do not commit, push, or overwrite an existing diff.

## Steps

### Step 1: Prepare the mapping backing before replacement

Refactor `sys_mmap` so every validation and backing acquisition that does not
require removal happens first. A local enum is acceptable to hold exactly one of:
anonymous-private/shared, prepared DRM `DeviceMappingSource`, or regular-file
`SharedFileMapping`. It must not become global state or a second owner.

Before any `unmap_user_mapping` call, complete:

- fixed zero-length and address alignment/null validation before potentially
  stateful regular page-cache backing acquisition;
- anonymous `fd == -1` and `offset == 0` validation;
- non-anonymous fd and aligned offset validation;
- OFD access-mode, inode kind, shared-write, and DRM flag/permission checks;
- DRM `file.mapping(...)` or regular `fs::mapping(...)` acquisition and errno
  conversion.

Keep ABI decoding and errno conversion in syscall. Do not copy file/device state
or leak concrete adapters into MemorySet.

**Verify**:
`nl -ba kernel/src/syscall/memory.rs | sed -n '75,185p'` → every listed
validation precedes the only fixed unmap call.

### Step 2: Perform fixed replacement immediately before dispatch

After cheap range validation and backing preparation, call `unmap_user_mapping`
once for `MAP_FIXED`, then immediately dispatch the prepared variant to the
existing map method. `MAP_FIXED_NOREPLACE` must never unmap and must continue
passing exact/no-replace semantics to MemorySet. Full user-range/resource
validation remains with MemorySet; do not duplicate its owner logic in syscall.

Do not add rollback of the old mapping if a later map call returns OOM or a
MemorySet error; that is a separate semantic design and is not required by the
demonstrated Linux ordering.

**Verify**:
`rg -n "unmap_user_mapping" kernel/src/syscall/memory.rs` → one mmap
replacement call plus the independent munmap syscall path, with no early call
above backing validation.

### Step 3: Tighten the ABI claim and run all gates

Update the mmap row in `docs/syscall-support.md` to state that invalid or
unauthorized fixed backing is rejected before overlap removal. Do not claim
transactional restoration. Run every command in the table.

**Verify**: all commands exit 0; `git diff --check` is empty.

## Test plan

With no host kernel test target and Make prohibited, statically trace these
cases: zero-length or invalid-address `MAP_FIXED`; fixed anonymous with non-`-1` fd;
fixed file with bad fd; write-only fd;
shared writable mapping from read-only fd; non-inode fd; invalid DRM sharing or
execute permission; backing setup OOM; valid fixed anonymous/file/device;
`MAP_FIXED_NOREPLACE` conflict. Every pre-dispatch error must occur before the
single unmap call.

## Done criteria

- [x] All ABI/backing errors listed above precede fixed unmap.
- [x] Valid MAP_FIXED still replaces overlaps once.
- [x] MAP_FIXED_NOREPLACE never unmaps.
- [x] No rollback, compatibility path, or new persistent state was introduced.
- [x] All static gates pass and only in-scope files changed.
- [x] `plans/README.md` says DONE.

## STOP conditions

- Preparing DRM/regular backing has externally visible mutation, not merely
  validation/reference acquisition.
- The refactor requires a concrete fs/DRM adapter inside MemorySet.
- Linux fixed baseline evidence contradicts prevalidation-before-replacement.
- The excerpt drifted or a gate fails twice.

## Maintenance notes

Reviewer focus is ordering, not just returned errno. Future mmap kinds must join
the same prepare-then-replace structure so a cheap validation error cannot
destroy an existing VMA.
