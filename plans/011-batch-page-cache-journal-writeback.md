# Plan 011: Batch page-cache journal writeback

## Problem

`CachedFile::writeback_range` scans resident pages in fixed 32-page batches, but
currently calls `Inode::write_storage` once per dirty page. The ext2 adapter starts
and commits a complete synchronous JBD2 transaction for every call. A fully dirty
32-page batch therefore performs 32 mutation snapshots, 32 inode rewrites, and
128 journal/device FLUSH barriers, followed by the range's final storage flush.
The page-cache batching limits temporary memory but does not batch the dominant
durability cost.

Linux v7.1's fixed [`fs/jbd2/transaction.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/jbd2/transaction.c)
reserves credits for multiple modified buffers in one handle and restarts a
handle only when a larger operation cannot extend the transaction. LiteOS keeps
its synchronous single-transaction journal model, but a page-cache scan batch
must likewise amortize commit barriers instead of treating every page as a
filesystem transaction.

## Ownership and interface

- `CachedFile` remains the unique owner of resident pages, dirty state, scan
  batching, and post-commit clean publication.
- `Inode::write_storage_batch` is the filesystem storage seam. The page cache
  supplies page images through a short-lived `StorageWriter`; it cannot obtain
  an ext2 `MutationGuard`, journal, block map, or commit adapter.
- The ext2 implementation owns one `MutationGuard` for all callback writes,
  stages all data and allocation metadata in the existing unique write-set,
  updates inode size/mtime/ctime once at the greatest written end, then commits.
- The default `Inode` implementation preserves existing adapters by delegating
  each callback write to `write_storage`; no read-only/volatile adapter gains a
  second mutation path.
- `JournalLayout` owns the immutable descriptor/tag and maximum unique-home-block
  facts derived from the validated JBD2 superblock. If the cached maximum is too
  high commit could exceed the journal; if too low writeback splits early. The
  formula is tested against the descriptor equation for all relevant boundaries.
- `Journal::stage` rejects a new unique home block before mutation of the
  write-set when the active transaction is full. Replacement of an already
  staged block updates its existing image in place and does not allocate.

No new global, lock, Atomic, persistent dirty flag, allocation map, or journal
owner is introduced. `JournalLayout` is immutable derived layout state stored by
the existing journal owner, not a second source of filesystem state.

## Transaction and failure semantics

- One page-cache scan batch first snapshots at most 32 dirty page Arcs, retaining
  the existing fixed stack bound and single PAGE_SIZE scratch buffer.
- The batch callback copies each page into that scratch and feeds it to the
  storage writer. Pages are marked clean only after the whole storage batch
  reports a successful commit.
- `NoSpace` from a multi-page batch triggers bounded binary backoff. The failed
  ext2 guard aborts all staged writes and restores allocator/inode state before
  a smaller prefix is retried. A single-page `NoSpace` is returned unchanged.
- After a successful prefix, only that prefix is marked clean. A later I/O or
  capacity failure leaves the uncommitted suffix dirty, matching the existing
  per-page partial-progress semantics.
- Journal capacity is checked when staging the first occurrence of a home block;
  a repeated inode/bitmap/indirect-block update reuses the existing image and
  cannot consume a second credit.

## Complexity and performance claim

Let `D <= 32` be dirty pages in one resident scan batch and `T` the number of
transactions after capacity backoff.

- before: `D` mutation snapshots, `D` inode finalizations, `D` JBD2 commits and
  `4D + 1` FLUSH barriers for the writeback range;
- after: `T` mutation snapshots/commits, one inode finalization per transaction,
  and `4T + 1` FLUSH barriers; on the normal journal where the full batch fits,
  `T = 1`, so 32 pages fall from 129 to 5 barriers;
- page cache retains exactly one PAGE_SIZE scratch copy per dirty page; a newly
  allocated complete ext2 data block is initialized directly from that buffer,
  while an existing aligned block directly replaces its journal image; neither
  path allocates, clears, or copies through a second block-sized RMW Vec;
- resident scan work remains O(resident pages visited), journal staging remains
  O(unique home blocks log unique home blocks), and no range-sized allocation is
  added;
- replacing an already staged home block changes from allocating/copying a new
  Vec plus tree replacement to one tree lookup and in-place block copy.

These are exact operation/barrier counts from the interfaces, not elapsed-time
benchmark claims.

The optimized RV64 image inlines `commit_with_backoff` (no standalone symbol).
`CachedFile::writeback_range` has a 5,824-byte frame containing the fixed page
scratch and 32 Arc slots; `Ext2Inode::write_batch` has a 3,360-byte frame. Both
remain below 5% of the 128 KiB kernel stack, and neither grows with file/range
length. This is frame-size evidence, not a whole-call-chain bound.

## Verification

- Host tests exercise the production backoff/layout seams: a 32-item batch uses
  one commit when it fits; constrained capacity preserves order and commits each
  item once; a later failure cleans only the committed prefix; single-item
  exhaustion propagates; JBD2 layout capacity matches brute-force descriptor
  accounting across 1/2/4 KiB blocks and boundary journal sizes.
- Static audit confirms only successful committed slices call
  `mark_clean_if_unmapped`, ext2 callback failure drops the live guard, and no
  page-cache caller obtains journal internals. Empty directory writes retain the
  pre-existing `IsDirectory`-before-empty-buffer error order.
- RV64 check/clippy, optimized assembly inspection, architecture fence, format,
  host unit tests, and `git diff --check`.
- Independent Standards and Spec reviews.

Do not run Make.

## Done criteria

- [x] A fitting 32-page dirty batch uses one ext2 journal commit.
- [x] Journal capacity pressure splits without poisoning the journal or losing dirty pages.
- [x] Ext2 finalizes the inode once per committed storage batch.
- [x] Re-staging one home block reuses its allocation and transaction credit.
- [x] Fixed scratch/batch memory and existing error/cleanup semantics remain intact.
- [x] Production policy tests, RV64 gates, contracts, and independent reviews pass.
