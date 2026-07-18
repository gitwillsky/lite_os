# Plan 009: Index file-descriptor free slots

## Problem

`FileDescriptorTable::allocate` scans the slot vector from `minimum`, and
`allocate_pair` scans from zero. Opening `N` descriptors into a dense table
therefore performs `0 + 1 + ... + (N - 1)` slot probes: O(N²). Repeated
`F_DUPFD(min)` has the same linear search cost per call.

Linux v7.1's fixed [`fs/file.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/file.c)
uses `next_fd`, an open-fd bitmap, and a summary bitmap to return the lowest
available descriptor at or above the requested minimum without rescanning the
pointer array. LiteOS must preserve that observable lowest-fd rule and its
existing `EBADF`/`EMFILE`/`ENOMEM` classification.

## Ownership and representation

- `fs::file::descriptor_table::FileDescriptorTable` remains the process-level
  owner of `FD_CLOEXEC` and descriptor publication. Its private `IndexedSlots<T>`
  is the unique compound owner of the slot vector and free-slot selection, so
  callers cannot mutate occupancy without the index transition.
- `IndexedSlots<T>` contains a private four-level `FreeSlotIndex` derived under
  the same files lock. Level zero has one set bit per empty materialized slot;
  each upper level has one set bit per nonempty child word.
- The index also owns one conservative `occupied_prefix`: every materialized fd
  below it is proved occupied. In-order publication advances it by one; detach
  lowers it; fork clones it with the bitmap. It may lag and cause a bounded
  bitmap query, but can never move past an unproved slot.
- `MAX_FILE_DESCRIPTORS == 1_048_576` and RV64 `usize::BITS == 64` prove four
  levels cover the complete domain: 1,048,576 → 16,384 → 256 → 4 → 1 words.
- Invalid/unmaterialized slots have no set bit. `IndexedSlots<T>` keeps the slot
  vector as semantic truth and the index as a search accelerator; neither
  representation crosses the fd-table module interface.
- Any mismatch is fail-stop: publication/removal methods assert the expected
  old index bit. There is no recovery path that guesses which copy is correct.

## Mutation transaction

- Growth reserves every required index level and the slot vector before either
  logical length changes. OOM leaves slots and index logically unchanged.
- Descriptor constructors run only after limit checks and all backing reserves,
  so failed insertion cannot publish or transiently increment descriptor refs.
- Publishing into an empty slot clears exactly one level-zero bit and propagates
  zero/nonzero transitions upward before the entry becomes visible.
- Detach, CLOEXEC detach, and close make the slot empty and set exactly one bit.
- `dup3` replacement leaves the bit unchanged; publication into a hole clears it.
- fork clones entries and index as one fallible transaction; failure drops the
  unpublished cloned descriptors and their reference increments.
- `take_all` transfers entries and index together and leaves a canonical empty
  table behind.

## Complexity and memory claim

- `first_free(minimum)` ascends at most four summary levels and descends at most
  three child levels: no more than seven bitmap-word loads and four
  `trailing_zeros` operations. Because the fd domain is fixed, lookup is strict
  O(1).
- `allocate_pair` performs two such lookups; ordinary open, pipe, socketpair,
  eventfd, epoll, dup, and `F_DUPFD(min)` no longer scan the slot vector.
- At the maximum table size the index has 16,384 + 256 + 4 + 1 = 16,645
  logical RV64 words. Their 133,160-byte payload plus four 24-byte Vec headers
  and the 8-byte prefix is 133,264 bytes. Against 1,048,576 16-byte
  `Option<FileDescriptor>` slots (16 MiB), the structural overhead is 0.79%;
  actual allocator capacity may be geometrically rounded.
- Growth touches only newly materialized bitmap words plus the slot initialization
  already required by the vector. Close and publication touch at most four words.

Opening the maximum 1,048,576 descriptors into the old dense table required
`N(N-1)/2 = 549,755,289,600` `Option<FileDescriptor>` probes. In the new dense
in-order case `occupied_prefix == entries.len()`, so each lookup terminates at
the bounds check without reading a bitmap word; publication performs only the
constant index transition plus amortized vector/bitmap growth. This is a
parametric instruction/work model, not a runtime benchmark claim.

## Verification

- Static cases: empty/dense/sparse tables; minimum inside and beyond current
  length; word boundaries 63/64/65 and hierarchy boundaries 4095/4096/4097;
  close/reopen lowest-fd reuse; pair allocation with zero/one/two holes; dup3
  empty/replacement/same-fd wrapper behavior; CLOEXEC batches; fork/exit/take-all;
  limit before growth; OOM before logical mutation.
- Host tests path-include the exact production `IndexedSlots<T>` implementation
  and cover hierarchy churn, pair allocation with zero/one/two holes, replacement
  into an occupied slot and a hole, conditional CLOEXEC-style detach, limits,
  sparse minimum growth, clone, iteration, and take-all.
- Source audit confirms `entries` is private to `IndexedSlots<T>` and every
  occupancy mutation has exactly one index mutation.
- RV64 check, clippy with warnings denied, optimized assembly inspection,
  architecture fence, format, and `git diff --check`.
- Independent standards and spec reviews.

Do not run Make.

## Done criteria

- [x] All fd allocation paths return the lowest free descriptor in range.
- [x] Allocation lookup never scans `entries`.
- [x] Slots and free index cannot diverge on success, error, clone, exec, or exit.
- [x] Index growth OOM leaves the published table unchanged.
- [x] Architecture ownership/interface/performance contracts are updated.
- [x] Static/RV64 gates and independent reviews pass.
