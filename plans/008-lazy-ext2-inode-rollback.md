# Plan 008: Lazily capture ext2 inode rollback state

## Problem

Before this plan, every ext2 mutation cloned every live entry in `inode_cache`
before it started the JBD2 transaction. A one-inode metadata update therefore
performed O(cached inodes) Arc clones, inode-lock acquisitions, copies, and temporary
allocation even though abort can only need the inodes actually changed by that
transaction.

This cost is unrelated to the operation and grows with filesystem working-set
history. It also turns ordinary metadata mutation into an avoidable allocation
failure surface.

## Ownership and interface

- `fs::ext2::journal::MutationGuard` remains the unique owner of runtime undo
  state and the active JBD2 transaction lifetime.
- `MutationGuard::inode` is the only mutable live-inode entry point inside an
  ext2 mutation. On first use it records the inode's Arc and disk snapshot in a
  fixed stack slot, then returns its disk lock. Repeated use is allocation-free.
- Newly allocated inode numbers and final-Drop inodes without an upgradable Arc
  are registered before mutation. Abort removes those cache entries; it does not
  copy state that did not exist or cannot remain live after the transaction.
- No persistent generation, dirty flag, bitmap, cache, Atomic, global, or second
  inode-state owner is introduced.

Superblock and group-descriptor rollback remain unchanged in this plan. Their
state is filesystem-topology sized; this plan removes the unbounded runtime
working-set term without mixing allocator rollback into the inode interface.

## Required migration

All paths that mutate `Ext2Inode::disk` while a transaction is active must obtain
the lock from `MutationGuard::inode` before their first write. Helper paths that
can allocate/free blocks or rewrite inode pointers take the guard explicitly so
the invariant remains local and mechanically reviewable. This includes:

- read-atime, write, append, truncate, fallocate, chmod/chown, and utimens;
- create, symlink, link, unlink, and rename;
- block-tree allocation/reclamation and orphan-chain publication/recovery;
- final `Ext2Inode::drop` reclaim.

Error, early-return, and commit failure paths keep the guard live so Drop aborts
the journal, restores tracked live inodes, restores superblock/groups, and
removes transaction-created cache entries.

## Performance claim

Let `C` be live cached inodes and `G` filesystem block groups. The current
single-transaction domain has a proved bound of four distinct live inodes:
rename may touch old parent, new parent, moved inode, and replacement inode.
Create/symlink or final Drop additionally uses at most one transient inode slot,
never in a transaction with four live owners.

| Transaction path | Maximum live undo slots | Transient slot |
|---|---:|---:|
| metadata/read-atime/write/append/truncate/fallocate | 1 | 0 |
| create/symlink | 1 | 1 |
| hard link/unlink | 2 | 0 |
| rename with replacement | 4 | 0 |
| orphan defer/recovery/final Drop | 2 | at most 1 |

The orphan row's two-live case is defer/recovery; final Drop uses one live
predecessor plus the transient self slot, so those maxima are not simultaneous.

- before: begin/undo storage and lock work is O(G + C);
- after: allocator snapshot remains O(G), while inode undo has fixed four-slot
  storage and at most four comparisons per access, strictly O(1) in C;
- common one-inode metadata update: inode rollback work falls from C snapshots to
  one snapshot;
- repeated edits do not allocate or snapshot twice.

On RV64 each `(Arc<Ext2Inode>, Ext2InodeDisk)` undo element occupies 136 bytes.
At one million cached inodes a one-inode transaction therefore avoids 999,999
inode locks/Arc clones and replaces 129.70 MiB of heap undo storage with a fixed
544-byte stack array, of which one slot is populated. This is a parametric model,
not a runtime benchmark claim.

The optimized RV64 assembly gives `MutationGuard::inode` a 208-byte frame and
contains no allocator call in that capture path. The largest directly observed
ext2 function frame is 5088 bytes (final-drop glue), below 4% of the 128 KiB
kernel stack; this is frame-size evidence, not a whole-call-chain bound.

The explicit guard parameter pushed the parent `ext2.rs` over its reviewed size
ratchet, so directory-entry insertion/removal moved into the existing
`ext2::directory` deep module. The parent shrank from 2255 to 2087 lines and its
architecture limit was lowered accordingly; no packed layout crosses the seam.

## Verification

- source audit: no active mutation path directly obtains a mutable inode disk
  lock; every such helper receives and uses `MutationGuard`;
- source audit: each new inode number is registered before `Ext2Inode::load` can
  publish it in the cache;
- static cases: allocator-snapshot OOM, failure before/after new inode load, repeated
  same-inode edits, rename touching several inodes, rollback Arc-neutral orphan
  decisions, the exact four-live/one-transient capacity proof, orphan predecessor
  update, and journal commit failure;
- `cargo fmt --all -- --check`;
- RV64 cargo check, clippy with warnings denied, and release assembly build;
- architecture fence and `git diff --check`;
- independent standards and spec reviews.

Do not run Make and do not commit.

## Done criteria

- [x] Normal ext2 mutation no longer scans or snapshots the full inode cache.
- [x] Every live inode mutation is captured before its first write.
- [x] Newly allocated inode cache entries are removed on abort.
- [x] Existing JBD2, allocator, error, namespace, orphan, and cache semantics remain.
- [x] Architecture contracts and interface baseline describe the new seam.
- [x] Static/RV64 gates and independent reviews pass.
