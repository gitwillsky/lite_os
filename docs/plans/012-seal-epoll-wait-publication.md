# Plan 012: Seal epoll wait publication

## Problem

`sys_epoll_pwait` builds source wait keys, then hands them to the indexed wait
registry. The registry-lock readiness closure currently calls the complete
`evaluate(epoll, 1)` path again. With `N` interests and no ready event, that
second pass allocates and clones an `N`-entry interest snapshot, allocates a
ready vector, builds the complete wait-key vector, and creates a temporary
`Vec<PollWaitKey>` for every interest. None of the second pass's keys survive;
the closure only keeps a boolean.

This is also a correctness defect under memory pressure. `evaluate` drains the
epoll notification token before its fallible snapshot/key allocations, while
the closure converts any allocation error to `false`. A ready source can
therefore be reported as not ready after its edge was consumed, allowing the
task to block without another wake.

The old key set also omits the top-level epoll notification pipe. If ADD/MOD/DEL
or final-close changes the interest set after the first snapshot, the registry
closure may observe that the changed interest is not ready yet, discard the new
keys produced by its second evaluation, and publish the stale key set. A later
edge on the changed interest then has no registered source key and cannot wake
the waiter. This contradicts the Phase 47 contract that ctl/close notification
wakes waiters built from an old snapshot.

Linux v7.1's fixed [`fs/eventpoll.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/eventpoll.c)
rechecks event availability in the wait publication loop and uses the eventpoll
wait queue to make ready-list/control changes visible to sleepers. LiteOS keeps
its source-indexed wait design, but the equivalent publication recheck must be
infallible and an interest-set change must invalidate the old source-key set.

## Ownership and interface

- `fs::Epoll` remains the unique owner of interests, delivery cursor,
  ET/ONESHOT state, and its coalesced ctl/close notification pipe.
- `Epoll::recheck_changed` is the only registry-lock snapshot validation seam.
  It atomically drains the ctl/close token and compares the Pipe's persistent
  read generation with the generation captured before that snapshot. A mismatch
  forces the outer loop to rebuild even when another waiter already consumed the
  coalesced token and no event is currently deliverable.
- `PipeEnd::drain_readiness` continues to clear the unique notification token,
  but returns the read generation observed under the same Pipe state lock.
  Existing notification users may ignore the result; the generation and token
  remain owned by the existing Pipe state, with no second flag or owner.
- `syscall::poll::wait_keys::PollWaitKeys` is the transient key-collection deep
  module. Callers add OFD interests or the top-level epoll change source; the
  module recursively expands concrete source adapters into one amortized key
  Vec and records epoll generation guards in one companion Vec. `finish`
  transfers both to wait orchestration; no per-interest key Vec survives.
- `syscall::poll::prepare_wait_sources` is the single recursive concrete-adapter
  preparation seam shared by poll, epoll, and direct blocking I/O. An initial
  nonblocking level evaluation may call it without a publication candidate;
  every invocation that precedes `wait_for_poll` runs after key generations are
  captured and before the registry lock. An explicit `PrepareIo` policy
  preserves epoll's `EIO` propagation and poll/direct wait's existing
  console-error behavior without duplicating the adapter switch.
- `syscall::epoll` remains responsible only for ABI/event evaluation and wait
  orchestration. It cannot inspect epoll state or notification occupancy.

No new global, lock, Atomic, persistent flag, readiness generation, wait
registry, or source state is introduced.

## Publication and failure semantics

1. Normal evaluation initializes one `PollWaitKeys` builder. For the top-level
   and every nested epoll it reserves guard/key capacity, drains the old token,
   captures the persistent read generation, adds the change source, and only
   then snapshots/expands interests. Change sources use an ungrouped normal
   registration so a ctl/close signal detaches every already-published stale
   waiter; ordinary data sources retain the top-level epoll instance wake-group.
2. All fallible snapshot/key allocation and recursive adapter preparation
   finishes before `wait_for_poll` enters the registry owner lock. Nested
   snapshot OOM returns `ENOMEM`; no Blocking state or wait membership has been
   published.
3. Under the registry lock, `PollWaitGuards::changed` performs no allocation and
   clones no Arc. It drains every captured epoll token and compares each current
   generation. A mismatch discards the stale prebuilt keys; the normal OFD/Epoll
   level projection independently detects a currently deliverable event.
4. If neither condition holds, the prebuilt keys include every top-level/nested
   change source. A concurrent ctl/close after the recheck therefore either
   blocks on the registry lock until membership publication and wakes it, or is
   observed as a generation mismatch by that snapshot. Because generation
   persists after token drain, two registry-external prebuilders cannot consume
   one another's only invalidation evidence.
5. Source readiness remains level-rechecked through the existing OFD seam;
   ET generation, ONESHOT disable, revision validation, delivery cursor,
   temporary signal-mask, timeout, and EFAULT-before-delivery semantics do not
   change.

## Complexity and performance claim

Let `N` be the number of interests scanned before sleep, `K` the expanded source
key count, and `D <= 5` the validated nesting depth.

- before publication: one ordinary evaluation plus a second complete
  evaluation under the wait-registry lock; the second pass clones `N` interests,
  constructs and discards `K` keys, and can allocate one temporary key Vec per
  visited interest/nested edge;
- after publication: one ordinary evaluation plus an allocation-free O(E + N)
  guard/level scan under the registry lock, where `E` is the number of expanded
  epoll edges; no snapshot, Arc clone, key construction, or allocator call
  occurs in the critical recheck;
- ordinary key construction changes from one transient Vec per interest/nested
  edge plus parent append/reserve to one amortized key Vec and one guard Vec per
  wait evaluation. Logical source traversal remains O(N + K), guard validation
  is O(E), and nested traversal remains bounded by D;
- the top-level notification adds exactly one pre-normalization key per epoll
  evaluation and closes the stale-key wakeup path.

These are interface/allocation counts, not elapsed-time benchmark claims.

## Verification

- Static audit proves every poll/epoll `wait_for_poll` closure calls only
  `PollWaitGuards::changed` plus owner-local level projections; guard validation
  contains no fallible operation, collection growth, Arc clone, snapshot, or
  callback into key construction.
- Static audit proves every top-level/nested epoll captures notification
  generation before snapshot/interest expansion, and all OFD/nested expansion
  appends into a single `PollWaitKeys` owner.
- RV64 check/clippy, optimized assembly inspection, architecture fence, format,
  host unit tests, and `git diff --check`.
- Independent Standards and Spec reviews.

Do not run Make.

## Done criteria

- [x] Registry-lock epoll recheck is allocation-free and cannot fold OOM into not-ready.
- [x] Top-level ctl/close notification invalidates stale source keys without a lost wake.
- [x] Poll/epoll key expansion uses one transient builder instead of per-interest Vecs.
- [x] LT/ET/ONESHOT, revision, cursor, nesting, timeout, signal-mask, and EFAULT semantics remain intact.
- [x] RV64 gates, contracts, static proofs, and independent reviews pass.
