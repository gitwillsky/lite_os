# Plan 013: Seal file-mapping arithmetic and fault preflight

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report; do not improvise. When done, update this plan's row in
> `plans/README.md` unless a reviewer told you that it maintains the index.
>
> **Drift check (run first)**:
> `git diff --stat e891a3f..HEAD -- kernel/src/syscall/memory.rs kernel/src/task/model.rs kernel/src/task/model/address_space.rs kernel/src/task/model/address_space/mapping.rs kernel/src/memory/mod.rs kernel/src/memory/mm.rs kernel/src/memory/mm/cow.rs kernel/src/memory/mm/mapping_request.rs kernel/src/memory/mm/private_area.rs kernel/src/memory/mm/shared_area.rs kernel/src/memory/mm/mmap.rs kernel/src/memory/mm/mmap/fault.rs kernel/src/memory/mm/mmap/advice.rs kernel/src/memory/mm/futex_key.rs kernel/src/fs/page_cache.rs tools/kernel-unit/src/lib.rs docs/architecture.md docs/architecture-contract.md docs/architecture-interface.txt docs/syscall-support.md`
> Plan 012 is expected to change the two architecture documents but not the
> memory sections described below. Compare the live excerpts and preserve all
> Plan 012 content. Any semantic drift in the symbols named below is a STOP
> condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: `plans/012-seal-epoll-wait-publication.md`
- **Category**: bug
- **Planned at**: commit `e891a3f`, 2026-07-15

### Scope revision (2026-07-15)

Independent review hit the original STOP condition: inode EOF may change while
the AddressSpace lock is held because truncate owns the filesystem operation
domain, so a size-only preflight cannot prove that a later private allocation is
SIGBUS-free. The plan is explicitly revised to include
`kernel/src/fs/page_cache.rs`: its existing per-inode operation lock now
provides a stable fault-page snapshot, checks EOF before cache/frame allocation,
and returns a transient page pin. The existing truncate callback also removes
EOF-external cached-private residents, matching Linux `even_cows=1` and covering
pages published after the snapshot but before callback lock acquisition. This
adds no lock, cache, flag or parallel owner; it closes the discovered race
instead of weakening the STOP condition.
The matching pinned source is Linux v7.1 `truncate_pagecache`, which uses
`unmap_mapping_range(..., even_cows=1)` and repeats it because private pages may
be COWed during truncation:
[`mm/truncate.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/mm/truncate.c#L780-L800).

## Why this matters

Regular-file mmap currently accepts a page-aligned offset without proving that
the rounded mapping range fits Linux's signed file-position ceiling. Later VMA
split, fault, futex-key and writeback code recomputes byte offsets with unchecked
addition/multiplication. A large offset can therefore wrap to an earlier file
page; for writable shared mappings that can expose or modify the wrong data.

The same fault path allocates and may globally reclaim a private frame before it
checks VMA permission or private-file EOF. Under pressure, a write to a read-only
lazy mapping or a fault beyond EOF can become OOM/SIGKILL instead of SIGSEGV or
SIGBUS. This plan establishes one validated page-index representation and makes
fault classification precede every residency allocation.

## Current state

- `kernel/src/syscall/memory.rs:129-174` validates only fd and byte alignment
  before a regular-file mapping is placed in `PreparedMapping`:

  ```rust
  if fd < 0 || !offset.is_multiple_of(crate::memory::PAGE_SIZE) {
      return -errno::EINVAL;
  }
  // ...
  PreparedMapping::SharedFile(mapping)
  ```

  This preflight is before destructive `MAP_FIXED` unmapping, so range overflow
  must also be rejected here with `EOVERFLOW`.

- `kernel/src/memory/mm/mapping_request.rs:15-52` carries a raw `u64` byte
  offset in `FileMappingSource`; its constructor cannot prove the mapping
  length:

  ```rust
  pub(crate) struct FileMappingSource {
      pub(super) mapping: Arc<dyn SharedFileMapping>,
      pub(super) offset: u64,
  }
  ```

- `kernel/src/memory/mm.rs:311-316` recomputes shared-file offsets during VMA
  partition with unchecked arithmetic. `mmap.rs:273-277,407-411` and
  `mmap/advice.rs:101-104` do the same before writeback. `futex_key.rs:65-69`
  already uses `checked_add` but still starts from the raw byte owner.

- `kernel/src/memory/mm/mmap/fault.rs:205-208` performs an unchecked page-index
  addition and multiplication before the EOF decision:

  ```rust
  let index = (shared.file_offset / config::PAGE_SIZE as u64)
      + (vpn.as_usize() - area.vpn_range.start.as_usize()) as u64;
  if index * config::PAGE_SIZE as u64 >= shared.mapping.size() {
      return Ok(PageFaultOutcome::BusError);
  }
  ```

- `kernel/src/memory/mm/mmap/fault.rs:58-95` calculates
  `needs_private_frame`, calls `allocate_private_frame()`, and only then checks
  VMA membership, U/R/W/X permission and private-file `faultable(vpn)`.
  `allocate_private_frame` invokes current-mm and global reclaim before it
  returns OOM. `kernel/src/trap/mod.rs:175-191` maps that OOM to process death.

- `kernel/src/memory/mm/private_area.rs:68-76` uses saturating arithmetic for
  cached-file EOF classification. Saturation prevents a panic but can silently
  change which file byte owns a virtual page; this must become checked arithmetic
  whose success follows from the published mapping invariant.

- The architecture contract requires syscall memory code to parse only Linux
  ABI/errno and makes `MemorySet` the sole VMA/split/PTE owner
  (`docs/architecture-contract.md:212-213`). The new page-index value is derived
  immutable VMA state under that owner, not a second mapping table or cache.

- The fixed primary source is Linux v7.1 commit
  [`8cd9520d...`, `mm/mmap.c::file_mmap_ok`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/mm/mmap.c#L266-L277).
  It rejects a file mapping with `EOVERFLOW` when the page offset plus aligned
  length exceeds `MAX_LFS_FILESIZE`. Use the repository's RV64/Linux baseline;
  do not copy a moving `master` implementation.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Host regression | `cargo test -p kernel-unit` | all tests pass |
| Scheduler regression | `cargo test -p scheduler-unit` | all tests pass |
| RV64 check | `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel` | exit 0 |
| RV64 lint | `cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings` | exit 0, no warnings |
| Architecture | `cargo run --quiet -p architecture-check` | exit 0 |
| Release assembly | `(cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm)` | exit 0 |
| Patch hygiene | `git diff --check` | no output |

Never run Make.

## Scope

**In scope** (the only implementation/doc files to modify):

- `kernel/src/syscall/memory.rs`
- `kernel/src/task/model.rs`
- `kernel/src/task/model/address_space.rs`
- `kernel/src/task/model/address_space/mapping.rs`
- `kernel/src/memory/mod.rs`
- `kernel/src/memory/mm.rs`
- `kernel/src/memory/mm/cow.rs`
- `kernel/src/memory/mm/mapping_request.rs`
- `kernel/src/memory/mm/private_area.rs`
- `kernel/src/memory/mm/shared_area.rs`
- `kernel/src/memory/mm/mmap.rs`
- `kernel/src/memory/mm/mmap/fault.rs`
- `kernel/src/memory/mm/mmap/advice.rs`
- `kernel/src/memory/mm/futex_key.rs`
- `kernel/src/fs/page_cache.rs`
- pure validated-range and fault-preflight helpers below `kernel/src/memory/mm/`
- `tools/kernel-unit/src/lib.rs`
- `docs/architecture.md`
- `docs/architecture-contract.md`
- `docs/architecture-interface.txt` (only through the official generator)
- `docs/syscall-support.md`
- `plans/README.md`

**Out of scope**:

- Anonymous, ELF or DRM mapping semantics except adapting a common type without
  changing their behavior.
- Stack-growth policy, reclaim policy, COW, ASIDs, TLB shootdown targeting or
  trap signal mapping.
- Raising the filesystem's maximum file size or adding a new mmap flag/type.
- Any new global, cache, lock, Atomic, flag or parallel mapping owner.

## Git workflow

- Work on `main` after Plan 012 has been reviewed, committed and pushed.
- Inspect the complete dirty tree before handoff and preserve unrelated changes.
- Phased commits and pushes are allowed only after the complete dirty tree and
  this logical change have been reviewed.

## Steps

### Step 1: Introduce a validated file page-range value

Replace the raw byte-offset-only construction with a small immutable value under
the memory owner. Its constructor must take the regular-file mapping, byte
offset and original mmap length, round length exactly as mmap does, and prove:

1. nonzero length and page-aligned offset;
2. rounded page count does not overflow;
3. the start page and page count fit the fixed Linux/RV64
   `MAX_LFS_FILESIZE` rule; and
4. any page index in the VMA converts back to a byte offset with checked
   arithmetic.

Return a distinct alignment/range error so `sys_mmap` maps alignment to EINVAL
and range overflow to EOVERFLOW. Construct this value while preparing the file
mapping, before the `if fixed { unmap_user_mapping(...) }` point. Pass the
validated source through Task/AddressSpace without reconstructing it from raw
offset and length.

Store a page index, not a byte value that every caller re-divides. Give it only
the scoped methods actually required by VMA split, fault, sync/advice and futex
key projection. If a `pub(crate)` or `pub(super)` seam is widened, document its
exact callers and failure consequence in `docs/architecture-contract.md`.

**Verify**:

```bash
rg -n "FileMappingSource::new|offset as u64" kernel/src/syscall/memory.rs kernel/src/task/model/address_space kernel/src/memory/mm
```

Expected: no raw reconstruction remains on the regular mmap path; all creation
uses the validated constructor before `MAP_FIXED` replacement.

### Step 2: Route every file-page calculation through the invariant

Change `SharedFileArea` and cached `PrivateFileArea` to retain the validated
start page (or an equivalent checked page-offset value). Use its methods in:

- `MapArea::partition_protectable` for middle/right split bases;
- `sync_shared_mapping`, unmap writeback and madvise discard writeback;
- shared-file fault page lookup and EOF classification;
- cached private-file `faultable`, `has_file_bytes` and `fill`; and
- shared-file futex byte-key projection.

Do not use saturating arithmetic to recover from an impossible mapping. Reject
untrusted range overflow at construction. For arithmetic that follows from the
published VMA invariant, use checked operations and return the existing internal
range/corruption error rather than wrapping. Compare EOF without multiplying an
unchecked index; a final partial file page remains faultable because its page
start is below file size.

**Verify**:

```bash
rg -n "file_offset.*\+|index \* config::PAGE_SIZE|source_offset.*saturating|saturating_add\(page_start" kernel/src/memory/mm
```

Expected: no unchecked/saturating regular-file VMA offset calculation remains.
Any match must be an unrelated anonymous/device value and explained in review.

### Step 3: Classify faults before allocating private residency

Extract a small production-used fault-preflight decision seam. It must inspect,
in this order, after the existing stack-growth attempt:

1. VMA membership and user accessibility;
2. the access-specific R/W/X permission;
3. private-file page faultability/EOF;
4. device/shared/private residency kind; and
5. whether a new private frame is actually required.

Only the final `NeedsPrivateFrame` decision may call
the page-cache fault snapshot seam. For cached files, that seam must serialize
with truncate using the existing per-inode operation lock, reject EOF before
cache node/shared frame/private frame allocation, and return a transient pinned
page. Only a successful snapshot may call `allocate_private_frame()`. Reacquire
the mutable VMA borrow after allocation and assert/recheck only the stable VMA
residency invariant while the `MemorySet` lock is still held; do not re-read live
EOF or copy VMA state into a second owner. SegmentationFault and BusError must be
allocation- and reclaim-free. A legitimate private page fault may still return
OOM and follow the existing SIGKILL policy.

Do not change `grow_stack_for_fault`; Linux-compatible grow-down ordering is not
part of this finding.

**Verify**:

```bash
sed -n '50,115p' kernel/src/memory/mm/mmap/fault.rs
```

Expected: permission/EOF decisions are visibly completed before the only call
to `allocate_private_frame`.

### Step 4: Add production-path host regressions

Include the pure page-range and fault-preflight modules from
`tools/kernel-unit/src/lib.rs` without duplicating their formulas in the test
crate. Add table-driven tests for:

- offset zero and the last valid page-aligned mapping;
- offset or rounded length one page beyond the Linux ceiling;
- addition crossing `u64::MAX`/`usize::MAX`;
- split at the first, middle and final VMA page preserving exact page identity;
- final partial file page versus first page wholly beyond EOF;
- PROT_NONE, write-to-read-only and execute-without-X decisions returning
  SegmentationFault and never `NeedsPrivateFrame`;
- private-file EOF returning BusError and never `NeedsPrivateFrame`; and
- a valid nonresident lazy private page returning `NeedsPrivateFrame`.

The tests must call the exact helpers used by production; a test-only duplicate
formula is not acceptable.

**Verify**: `cargo test -p kernel-unit` -> all existing and new tests pass.

### Step 5: Update fixed ABI and interface facts

Update the mmap row in `docs/syscall-support.md` to list `EOVERFLOW` and state
that regular-file page ranges are validated before destructive replacement.
Update `docs/architecture-contract.md` with the new immutable page-range owner,
its scoped callers and the rule that permission/EOF preflight precedes residency
allocation/reclaim. Do not edit a declaration to hide an implementation
shortfall; the code and tests must land first.

**Verify**: `cargo run --quiet -p architecture-check` -> exit 0.

### Step 6: Run the full non-Make gate and review

Run every command in "Commands you will need", inspect optimized RV64 assembly
for accidental panic/overflow paths in the file-range helper, then request an
independent Standards review and Spec review. Fix every blocker before handoff.

**Verify**: all commands exit 0, both reviews report no blocker, and
`git diff --check` prints nothing.

## Test plan

- Host tests live in the production helper modules and are compiled through
  `tools/kernel-unit`; do not add a parallel algorithm under `tools/`.
- The regression oracle is classification and exact page arithmetic, not wall
  time. The performance guarantee is structural: invalid permission/EOF paths
  cannot reach allocation/reclaim.
- Existing scheduler-unit, RV64 check/clippy, architecture and release assembly
  gates must remain green.

## Done criteria

- [x] Oversized regular-file mapping ranges return EOVERFLOW before MAP_FIXED removes anything.
- [x] VMA split, sync, advice, fault and futex-key code cannot wrap a file page/byte offset.
- [x] Invalid permission faults return SegmentationFault without allocation/reclaim.
- [x] Private-file EOF returns BusError without allocation/reclaim.
- [x] Concurrent truncate is serialized before cache/private frame allocation; a successful fault holds a transient page pin.
- [x] Truncate invalidation removes EOF-external shared and cached-private residents, including a page published by an earlier fault snapshot.
- [x] Final partial file pages remain faultable; wholly beyond-EOF pages do not.
- [x] Production-path host regressions cover every boundary listed above.
- [x] No new global, lock, Atomic, cache, duplicate state owner or private ABI exists.
- [x] All non-Make gates and independent reviews pass.
- [x] The complete dirty tree and this logical change passed review before phased submission.

## STOP conditions

Stop and report instead of improvising if:

- Plan 012 is not committed or its live edits overlap a memory contract section
  in a way that cannot be reconciled without replacing content.
- The fixed Linux v7.1/RV64 `MAX_LFS_FILESIZE` value cannot be established from
  the pinned source; do not invent a smaller private limit.
- Correct construction would require validating against current inode size.
  mmap beyond current EOF is legal; access, not mapping publication, produces
  SIGBUS.
- A solution requires a second VMA/file-offset table, persistent flag, cache,
  global or new lock.
- Permission can change while the MemorySet lock is held, or EOF cannot be
  stabilized by the existing page-cache operation domain without adding a new
  owner, invalidating the preflight-to-allocation proof.
- Any verification step fails twice after a reasonable correction.
- The change needs files outside Scope.

## Maintenance notes

- Future VMA operations must consume the validated page-index methods; never
  reconstruct byte offsets with `base + delta * PAGE_SIZE`.
- Reviewers should scrutinize EINVAL/EOVERFLOW ordering before MAP_FIXED,
  last-partial-page EOF semantics, and the absence of allocator calls on every
  SegmentationFault/BusError branch.
- ASID/range-targeted TLB invalidation is deliberately deferred. This plan only
  makes mapping/fault semantics correct and keeps current flush behavior.
