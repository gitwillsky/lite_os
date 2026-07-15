# Plan 001: Authorize chmod and chown inside the inode mutation

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving on. Do not
> run Make, create commits, or overwrite unrelated working-tree changes. Update
> this plan's row in `plans/README.md` when done.
>
> **Drift check (run first)**:
> `git diff --stat d4e59a8..HEAD -- kernel/src/syscall/fs/attributes.rs kernel/src/fs/permission.rs kernel/src/fs/inode.rs kernel/src/fs/ext2.rs kernel/src/fs/ext2/metadata.rs docs/architecture.md docs/architecture-contract.md docs/architecture-interface.txt docs/syscall-support.md`
> If an in-scope implementation file changed, compare the excerpts below with
> live code. Documentation already has unrelated WIP; merge into it. A semantic
> mismatch is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `d4e59a8`, 2026-07-15

## Why this matters

`chmod` and `chown` currently authorize against one metadata snapshot and mutate
later under the ext2 mutation lock. A concurrent ownership change can therefore
let a former owner apply a stale-authorized mode, including set-ID bits, to a
newly owned file. Authorization, set-ID normalization, and persistence must use
one live inode state under the filesystem's unique mutation owner.

## Current state

- `kernel/src/syscall/fs/attributes.rs:42-57` reads metadata, checks the caller,
  then calls `set_owner_mode` separately. `chown_inode` repeats this split at
  lines 90-115.
- `kernel/src/fs/ext2/metadata.rs:54-74` only acquires the mutation and inode locks
  inside `update_owner_mode`, after authorization is over.
- `kernel/src/fs/inode.rs:211-223` exposes precomputed `mode/uid/gid`, so an inode
  adapter cannot revalidate the caller against live metadata.
- `kernel/src/fs/permission.rs:5-48` already owns the filesystem-domain
  `AccessIdentity`; pass this value, never a `TaskControlBlock`, through the inode
  seam.
- Contract: syscall modules own ABI/errno only; ext2 owns its single mutation
  lock and inode disk state. Do not create a second authorization cache/version.

Current vulnerable shape:

```rust
// kernel/src/syscall/fs/attributes.rs:43
let metadata = inode.metadata()?;
if identity.uid() != 0 && identity.uid() != metadata.uid { ... }
inode.set_owner_mode(Some(mode), None, None)
```

```rust
// kernel/src/fs/ext2/metadata.rs:60
let mutation = self.fs.begin_mutation()?;
let mut disk = self.disk.lock();
```

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Check | `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel` | exit 0 |
| Lint | `cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings` | exit 0 |
| Architecture | `cargo run --quiet -p architecture-check` | exit 0, no diagnostics |
| Release link | `cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm` | exit 0 |
| Diff hygiene | `git diff --check` | no output |

## Scope

**In scope**:

- `kernel/src/fs/permission.rs`
- `kernel/src/fs/inode.rs`
- `kernel/src/fs/ext2/metadata.rs`
- `kernel/src/fs/ext2/journal.rs`
- `kernel/src/fs/ext2.rs`
- `kernel/src/syscall/fs/attributes.rs`
- `docs/architecture.md`
- `docs/architecture-contract.md`
- `docs/architecture-interface.txt`
- `docs/syscall-support.md`
- `plans/README.md`

**Out of scope**:

- Namespace/create/access permission rules.
- Credential storage or capability expansion.
- Other filesystems gaining writable metadata.
- Any compatibility method retaining `set_owner_mode(mode, uid, gid)`.

## Git workflow

- Work on the current branch; do not create a branch, commit, push, or open a PR.
- Preserve all pre-existing changes. Use `git diff -- <path>` before editing a
  file that was already dirty.

## Steps

### Step 1: Define a filesystem-domain owner/mode operation

In `fs/permission.rs`, add a small semantic request enum (for example
`OwnerModeChange`) with separate `Chmod { mode, identity }` and
`Chown { uid, gid, identity }` variants. `uid/gid` are `Option<u32>`; the syscall
layer must translate the ABI's `u32::MAX` sentinel. Keep `AccessIdentity` as the
only credential snapshot. Export the request through `fs/mod.rs` only if needed
by the syscall caller.

Replace `Inode::set_owner_mode` with one method accepting that request. Update
its contract to state that authorization and mutation are atomic against the
inode's live owner/mode. Do not leave the old method as a compatibility entry.

**Verify**: `rg -n "set_owner_mode" kernel/src` → no matches.

### Step 2: Implement the operation under the ext2 owner locks

In `Ext2Inode::update_owner_mode`, acquire the mutation lock and read `self.disk`
before checking permissions. Keep the rules exclusively in `fs::permission`:
ext2 supplies a live state value and only persists the returned update. Use a
guard prepare seam that starts rollback snapshot/journal only after authorization
succeeds, while retaining the same mutation lock through commit. Compute:

- chmod: non-root must equal live UID; a non-root caller outside the live GID
  loses `S_ISGID`; replace only low mode bits and preserve inode type.
- chown: each requested UID/GID is authorized independently against live state.
  A non-root owner may request its unchanged UID, and may select either the
  unchanged live GID or one of its groups; root may select either. A
  sentinel-only request therefore performs no UID/GID authorization check.
  For every non-directory chown request, including sentinel-only, clear SUID;
  clear SGID when group-execute is set or the non-root caller is outside the
  live GID. If a sentinel-only call actually drops either bit, treat that
  implicit mode change as requiring live owner/root authorization. Apply this
  normalization in the same metadata write.
- permission failure returns `FileSystemError::PermissionDenied`; conversion
  overflow/unsupported values retain their existing error mapping.

Write inode disk and ctime once, then commit the same mutation. There must be no
metadata read for authorization before `begin_mutation`.

**Verify**:
`rg -n "metadata\(\).*set_owner|set_owner_mode" kernel/src/syscall/fs/attributes.rs kernel/src/fs`
→ no split check/mutation path and no old symbol.

### Step 3: Reduce syscall code to ABI translation and errno mapping

Change `chmod_inode` and `chown_inode` to snapshot effective
`AccessIdentity`, translate raw mode/UID/GID arguments into the semantic request,
call the inode once, and map `FileSystemError` through `ferr`. Remove all
permission and set-ID calculations based on `inode.metadata()`.

**Verify**: `rg -n "inode\.metadata\(\)" kernel/src/syscall/fs/attributes.rs`
→ no output.

### Step 4: Ratchet contracts and claims

Update architecture/interface documents for the renamed scoped inode method and
its callers. In `docs/syscall-support.md` retain Complete status only after the
atomic authorization rule is stated. Do not edit baselines to hide an unintended
visibility expansion; the new request should remain `pub(crate)` at most and the
inode method should have only the existing filesystem/syscall callers.

**Verify**: `cargo run --quiet -p architecture-check` → exit 0.

### Step 5: Run all static gates

Run every command in the table, including the release link from `kernel/`.

**Verify**: all commands exit 0; `git diff --check` emits nothing.

## Test plan

The kernel package intentionally has no ordinary host unit-test target, and Make
is prohibited for this work. Review the implementation against these explicit
cases during static inspection: former-owner chmod racing root chown; non-root
chmod with/without group membership; root chmod; no-op and real chown; set-ID
clearing based on the live mode; permission failure before any disk write. The
compile, Clippy, architecture, and release-link gates are mandatory.

## Done criteria

- [x] Syscall attributes contain no `inode.metadata()` authorization.
- [x] The ext2 mutation lock covers live authorization through journal commit;
  the inode disk lock covers the live read and state write, and is released
  before rollback snapshot/commit I/O to avoid self-deadlock.
- [x] The old precomputed `set_owner_mode` interface is gone.
- [x] Permission failures map to `EPERM`; existing read-only/overflow/I/O errno
  behavior remains.
- [x] All commands in the command table pass without Make.
- [x] Only in-scope files changed and `plans/README.md` says DONE.

## STOP conditions

- Another writable inode implementation exists and cannot enforce the semantic
  request atomically.
- Correct locking would require a Task/credentials dependency inside ext2.
- The live disk UID/GID representation cannot express the request without a new
  ABI decision.
- An in-scope implementation excerpt has drifted or a gate fails twice.

## Maintenance notes

Reviewers should reject any later path that precomputes authorized owner/mode
values outside the inode mutation. Future ACL, idmapped-mount, or capability work
must extend the semantic operation; it must not add a parallel authorization
owner in syscall code.
