# Plan 017: Enforce ext2 directory link limits transactionally

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and stop on any listed STOP condition; do not improvise.
> Update this plan's row in `plans/README.md` when complete unless a reviewer
> owns the index.
>
> **Drift check (run first)**:
> `git diff --stat e891a3f..HEAD -- kernel/src/fs/ext2.rs kernel/src/fs/ext2/directory.rs kernel/src/fs/ext2/link_count.rs kernel/src/fs/ext2/journal.rs kernel/src/fs/mod.rs kernel/src/fs/vfs/mutation.rs kernel/src/syscall/fs/pathname.rs tools/kernel-unit/src/lib.rs docs/architecture.md docs/architecture-contract.md docs/architecture-interface.txt docs/syscall-support.md plans/README.md`
> Plans 012-016 may have changed the documents but not ext2 namespace mutation.
> Preserve all later content. Drift in `Ext2Inode::{create,unlink,rename}` or
> `directory::link` is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: `plans/012-seal-epoll-wait-publication.md`
- **Category**: bug
- **Planned at**: commit `e891a3f`, 2026-07-15

## Why this matters

LiteOS correctly rejects an ordinary ext2 hardlink when the target inode has
32,000 links, but mkdir and cross-parent directory rename increment the parent
directory's on-disk `u16` link count without checking the ext2 limit or integer
overflow. Repeated subdirectory creation/movement can therefore publish invalid
metadata and eventually wrap the count rather than returning EMLINK.

This plan centralizes ext2 link-count arithmetic, validates the net parent-link
delta before the first namespace mutation, and updates each parent exactly once
inside the existing journal transaction. It preserves replacement-directory
renames at a saturated destination because their new-parent delta is zero.

## Current state

- `kernel/src/fs/ext2/directory.rs:3` defines `EXT2_LINK_MAX: u16 = 32_000`.
  Ordinary hardlink creation at lines 344-356 checks
  `target_disk.i_links_count >= EXT2_LINK_MAX` before `+= 1`.

- `kernel/src/fs/ext2.rs:1692-1752` creates a child in one `MutationGuard`, but
  after allocating/writing the inode and directory entries it performs:

  ```rust
  let mut parent = mutation.inode(self)?;
  if kind == InodeType::Directory {
      parent.i_links_count += 1;
  }
  ```

  There is no parent limit check before allocation or namespace publication.

- `Ext2Inode::unlink` at `ext2.rs:1810-1815` decrements a directory parent's
  count with unchecked `-= 1`.

- `Ext2Inode::rename` validates names, ancestry, replacement type and directory
  emptiness before the first `remove_dir_entry_locked` at line 1890. For a
  cross-parent directory it later executes unchecked old-parent decrement and
  new-parent increment at lines 1924-1934. If the replacement is a directory,
  line 1894 first decrements the new parent, so the transaction's net new-parent
  delta is zero; otherwise it is +1.

- `MutationGuard` owns the only ext2 mutation lock, journal lifetime and four
  live-inode undo slots. Rename already covers old parent, new parent, moved
  inode and replacement inode. A link-count plan must be stack-local derived
  data inside that transaction, not another persistent owner or a wider undo
  set (`docs/architecture-contract.md:199`).

- `FileSystemError::TooManyLinks` already maps to EMLINK in
  `kernel/src/syscall/fs/pathname.rs:12-30`. No new errno or syscall adapter is
  needed. VFS owns permissions/path/cross-filesystem policy; ext2 owns its
  filesystem-specific link-count limit.

- The fixed Linux v7.1 references are
  [`fs/ext2/super.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/ext2/super.c)
  (publishes `EXT2_LINK_MAX` as the filesystem limit) and
  [`fs/namei.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/namei.c)
  (`vfs_mkdir` and `vfs_rename` reject a positive net parent increment at the
  limit before calling the filesystem mutation). LiteOS places the equivalent
  filesystem-specific check inside its unique ext2 mutation domain.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Link-policy tests | `cargo test -p kernel-unit` | all tests pass |
| Scheduler regressions | `cargo test -p scheduler-unit` | all tests pass |
| RV64 check | `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel` | exit 0 |
| RV64 lint | `cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings` | exit 0 |
| Architecture | `cargo run --quiet -p architecture-check` | exit 0 |
| Release assembly | `(cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm)` | exit 0 |
| Patch hygiene | `git diff --check` | no output |

Never run Make.

## Scope

**In scope**:

- `kernel/src/fs/ext2.rs`
- `kernel/src/fs/ext2/directory.rs`
- `kernel/src/fs/ext2/link_count.rs` (new pure checked policy module)
- `tools/kernel-unit/src/lib.rs`
- `docs/architecture.md`
- `docs/architecture-contract.md`
- `docs/architecture-interface.txt` (generated scoped-interface baseline)
- `docs/syscall-support.md`
- `plans/README.md`

`kernel/src/fs/ext2/journal.rs`, `kernel/src/fs/mod.rs`,
`kernel/src/fs/vfs/mutation.rs` and `kernel/src/syscall/fs/pathname.rs` are
read-only references unless a comment/visibility adjustment is strictly needed.

**Out of scope**:

- ext4 `dir_nlink`, indexed directories, raising `EXT2_LINK_MAX`, hardlink
  permission policy or a new filesystem feature bit.
- VFS ownership changes, journal capacity expansion, a fifth live-inode undo
  slot, or a new namespace transaction.
- Repairing an already-corrupt on-disk link count; detect impossible underflow
  as `InvalidFileSystem` and leave fsck/recovery policy unchanged.
- New global, lock, Atomic, cache, flag or duplicate link counter.

## Git workflow

- Work on `main` after Plan 012 is reviewed, committed and pushed.
- Inspect the complete dirty tree before handoff and preserve unrelated changes.
- Phased commits and pushes are allowed only after the complete dirty tree and
  this ext2 change have been reviewed.

## Steps

### Step 1: Centralize checked ext2 link-count policy

Move the single `EXT2_LINK_MAX = 32_000` fact into a small production module
under ext2. Add allocation-free helpers that operate on plain counts and return
a small domain error which the adapter maps as follows:

- positive delta at/above 32,000 -> `FileSystemError::TooManyLinks`;
- subtraction underflow/impossible count ->
  `FileSystemError::InvalidFileSystem`; and
- valid transition -> exact resulting `u16`.

Add a directory-rename planner that receives old/new parent counts, whether the
parents differ, and whether an existing destination directory is replaced. It
must return:

- for cross-parent moves, old parent minus one and new parent plus one when no
  directory is replaced, otherwise unchanged; and
- for same-parent replacement of one directory by another, the shared parent
  minus one because the namespace loses one child directory.

The returned plan is transient stack state under the current `MutationGuard`;
it is not an owner. Ordinary hardlink creation must use the same maximum fact
and retain its current TooManyLinks-before-entry-mutation ordering.

**Verify**:

```bash
rg -n "EXT2_LINK_MAX|i_links_count [+-]=" kernel/src/fs/ext2.rs kernel/src/fs/ext2
```

Expected: one constant; no unchecked parent/hardlink link-count arithmetic.

### Step 2: Reject saturated mkdir before namespace mutation

In `Ext2Inode::create`, preserve current error precedence:

1. inode type/name/kind validation;
2. `begin_mutation` and existing-name detection (EEXIST still wins);
3. for directory creation only, read the live parent count while the mutation
   lock is held and compute the checked +1 result;
4. only then allocate the new inode, write `.`/`..`, and add the parent entry;
5. after successful namespace preparation, assign the precomputed parent count
   once through `mutation.inode(self)` and persist it with mtime/ctime.

The EMLINK path may drop an otherwise empty guard, but it must not allocate an
inode, add an entry, change live link count or require rollback of a partial
namespace mutation. Do not use saturating arithmetic.

**Verify**:

```bash
sed -n '1690,1755p' kernel/src/fs/ext2.rs
```

Expected: the checked parent result is computed after EEXIST detection and
before `allocate_inode`/`add_dir_entry_locked`, then published once.

### Step 3: Plan directory rename deltas before the first edit

In `Ext2Inode::rename`, complete all current target/type/emptiness checks and
obtain source metadata before `remove_dir_entry_locked`. If the source is a
directory, read the live parent count(s) under the existing mutation lock and
call the pure planner with
`replacing_directory = existing target is a directory`.

This must produce:

- EMLINK before any entry/inode mutation when the destination needs +1 and is
  already at 32,000;
- success at 32,000 when replacing a directory (net destination delta zero);
- checked old-parent decrement; and
- one shared-parent decrement for same-parent directory replacement; and
- no parent-link change for same-parent non-replacement or non-directory rename.

Remove the intermediate destination-parent `-= 1` at replacement cleanup and
the later `-= 1`/`+= 1` pair. After directory-entry and `..` updates succeed,
assign each planned parent result exactly once through its existing undo owner,
then persist normal timestamps. Preserve the four-inode transaction bound and
all replacement reclaim behavior.

**Verify**:

```bash
rg -n "i_links_count [+-]=|link_count_plan|parent.*links" kernel/src/fs/ext2.rs
```

Expected: rename uses a precomputed net plan and has no transient unchecked
parent decrement/increment.

### Step 4: Check directory-unlink decrement and all touched failure paths

Route rmdir's parent decrement through the same checked helper. A valid remove
decrements once. An impossible zero count returns InvalidFileSystem before an
unchecked wrap; the existing `MutationGuard` restores any earlier staged entry
or inode change on error.

Audit create/rename/unlink exit paths:

- EMLINK is decided before the first namespace edit for create and rename;
- I/O/OOM after mutation still rolls back every captured live inode and journal
  image;
- replacement-directory net-zero plan does not double-decrement;
- timestamps are written with the final count; and
- no error path commits a partial count or orphan state.

Do not broaden this step into general corrupt-filesystem repair.

**Verify**: `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel`
-> exit 0.

### Step 5: Add boundary-complete production policy tests

Include the pure ext2 link-count module in `tools/kernel-unit` and test:

- 31,999 + 1 -> 32,000;
- 32,000 + 1 -> TooManyLinks;
- 0 - 1 -> Corrupt/InvalidFileSystem mapping;
- a normal cross-parent directory move decrements old and increments new;
- saturated new parent without replacement -> TooManyLinks;
- saturated new parent replacing a directory -> unchanged/success;
- same-parent directory replacement -> one shared-parent decrement;
- same-parent non-replacement and non-directory rename -> no parent delta; and
- repeated create/remove plans never exceed or wrap the u16/32,000 bound.

Tests must call the exact production helper, not reproduce arithmetic in
`tools/`. A 32,000-directory QEMU loop is not required; the journal/namespace
ordering proof is structural and the arithmetic boundary is deterministic.

**Verify**: `cargo test -p kernel-unit` -> all existing and new tests pass.

### Step 6: Update contracts and run all gates

Update `docs/architecture-contract.md` to state that ext2's transaction owns
the unique link count, computes net directory-parent deltas before namespace
mutation, and assigns each parent once. Update mkdir/link/rename completion text
in `docs/syscall-support.md` to include EMLINK at the ext2 limit without claiming
new filesystem features.

Run every command above, inspect release assembly only to confirm checked
arithmetic did not create panic paths, and request independent Standards and
Spec reviews. Resolve all blockers before handoff.

**Verify**: every command exits 0, `git diff --check` is silent, and both reviews
report no blocker.

## Test plan

- `kernel-unit` exhaustively covers the pure transition boundaries and net
  rename delta used by production.
- Static review verifies EMLINK is computed before `allocate_inode`,
  `remove_dir_entry_locked` or `add_dir_entry_locked` on the affected paths.
- Existing journal, BusyBox namespace and architecture gates remain regressions;
  this plan does not modify their verification fences.

## Done criteria

- [x] mkdir at parent link count 32,000 returns EMLINK before namespace mutation.
- [x] Cross-parent directory move needing +1 returns EMLINK at a saturated destination.
- [x] Replacing a destination directory is allowed at saturation because net delta is zero.
- [x] Parent increments/decrements and ordinary hardlinks use one checked ext2 limit fact.
- [x] Each rename parent publishes its final count once in the existing transaction.
- [x] Underflow is reported as invalid filesystem; no u16 wrap/saturation is used.
- [x] No extra undo slot, owner, global, lock, Atomic, cache or flag exists.
- [x] Host/RV64/architecture/release gates and independent reviews pass.
- [x] The complete dirty tree and this logical change pass review before phased submission.

## STOP conditions

Stop and report if:

- Plan 012 is uncommitted or ext2 namespace code no longer matches the current
  state excerpts.
- The fixed Linux v7.1 ext2 maximum is not 32,000 for the repository's supported
  feature set; do not silently adopt ext4 `dir_nlink` semantics.
- Correct planning would require a fifth live inode undo slot, a second
  transaction or link-count state outside the current parent inodes.
- The destination-parent delta for replacement directories cannot be known
  before the first namespace mutation.
- Preserving current EEXIST/ENOTEMPTY/type error precedence conflicts with early
  EMLINK validation; report the exact conflict rather than reorder blindly.
- Any gate fails twice after a reasonable correction or files outside Scope are
  required.

## Maintenance notes

- New ext2 namespace operations that change directory parentage must use the
  same net-delta helper before their first edit.
- Reviewers should focus on error precedence, saturated replacement rename,
  one-time parent publication and rollback slot count.
- Raising the limit or supporting ext4-style unlimited directory links requires
  an explicit on-disk feature/ABI plan; changing this constant alone is unsafe.
