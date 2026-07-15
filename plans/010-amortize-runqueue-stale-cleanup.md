# Plan 010: Amortize runqueue stale-generation cleanup

## Problem

Ready migration and stop/continue invalidate an old runqueue token by changing
the `SchedulingState` generation. The current cleanup restores the physical
queues to the logical state by scanning every local `BinaryHeap` before every
enqueue and scanning the complete remote mailbox before every delivery. Enqueueing
`N` distinct runnable tasks into an initially empty queue therefore performs
`0 + 1 + ... + (N - 1) = N(N-1)/2` scheduling-state lock/check operations in
addition to the required heap insertion work. This turns wake and fork bursts
into O(N²) scheduler work even when no stale generation exists.

Linux v7.1's fixed [`kernel/sched/core.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/kernel/sched/core.c)
and [`kernel/sched/fair.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/kernel/sched/fair.c)
keep runnable accounting in the runqueue ownership transaction and maintain the
ordered fair queue incrementally. LiteOS keeps its simpler generation-token
model, but routine enqueue must likewise be incremental rather than a hidden
whole-runqueue validation pass.

## Ownership and representation

- `SchedulingState::run_state` remains the unique owner of whether a Thread is
  Ready and of its CPU/generation. Physical heap/mailbox entries remain tokens
  validated against that owner; they never become a second membership owner.
- The existing per-hart `queued_entries` atomic is renamed `ready_entries` and
  becomes an exact derived count of `RunState::Ready { cpu }` transitions for
  that hart, including both local and inbound physical locations. The scheduling
  lock serializes every increment/decrement/move with the authoritative state
  transition. It is only a load/tick projection and cannot publish a task.
- `transition_to_ready` returns a non-Copy linear `ReadyTransition` borrowing
  the mutated `SchedulingState`; only the processor commit seam can consume it.
  The guard cannot be released first, and dropping the token uncommitted is
  fail-stop, so a future Ready ingress cannot silently omit or defer its count.
- Ready→Running/Stopped methods return the symmetric borrowing
  `ReadyRetirement`; both existing Ready egress paths must prove their decrement
  through the same linear-token interface.
- The separate physical `inbound_entries` count is deleted. Selection and timer
  preemption use `ready_entries`; stale physical tokens therefore cannot create
  phantom load or repeated single-task self-preemption.
- A private generic preallocated-heap internal seam owns the capacity-pressure
  policy. Its ordinary `make_room(1)` path observes existing spare capacity and
  does not call the generation predicate. Only a request that would exceed the
  boot-reserved capacity performs a full in-place retain. Invalid roots are
  removed independently so the published vruntime floor remains live.

## Mutation transaction

- New/continued/woken/preempted publication changes state to Ready and increments
  the selected hart count under the same scheduling lock before delivering the
  token. Delivery failure is already fail-stop.
- Ready selection and Ready→Stopped decrement the old hart under the same lock.
  Ready affinity migration publishes the target before retiring the source;
  a Relaxed reader may conservatively observe one transient extra count between
  those two atomics, but never a stable missing or duplicated membership.
- Local enqueue reserves one physical slot, pushes, removes stale heap roots,
  then publishes the live minimum vruntime. Full retain is capacity-driven only.
- Remote delivery ordinarily performs one `VecDeque::push_back`; only a full
  mailbox triggers a retain before the capacity assertion.
- Mailbox drain performs one full validation pass because every entry must
  already be visited for transfer. It compacts the local heap only when the
  surviving batch would exceed its reserved capacity, then pushes the batch and
  prunes consecutive stale roots; a survivor can be revisited only if it becomes
  scheduling-visible at the root, never by a second batch scan. No runtime
  allocation is introduced.
- Physical stale tokens are bounded by one boot-reserved queue/mailbox capacity.
  Capacity is derived from the maximum number of fixed kernel stacks; after a
  retain, more current Ready tokens than capacity is an owner-invariant failure.

## Complexity and performance claim

- Enqueueing `N <= capacity` distinct valid tasks into an empty local queue makes
  zero full-queue predicate calls and costs the required O(N log N) heap work,
  replacing `N(N-1)/2` task-lock validations.
- Delivering the same burst remotely makes zero pre-delivery mailbox scans and
  O(N) deque work. Drain performs one O(N) validation/transfer pass instead of
  the prior O(N²) delivery validation plus drain.
- Stale heap roots cost O(log N) each when they become scheduling-relevant.
  Non-root tombstones are bulk-compacted only under capacity pressure. Exact
  logical `ready_entries` prevents them from affecting CPU selection or tick
  preemption while they await reclamation.
- No new allocation, lock, atomic, persistent cache/flag, or global is added. One
  existing atomic is repurposed and one existing atomic is removed. The linear
  stack token's private consumed bit exists only to fail-stop an omitted commit;
  it does not survive the scheduling transaction or own runnable state.
- RV64 release code has no standalone `make_runqueue_room` fast-path function:
  capacity arithmetic is inlined, while heap compaction, multi-root pruning, and
  full-mailbox compaction are emitted into `.text.unlikely`. The linear token's
  committed-bit checks are optimized out of every currently proven success path.
  Splitting local/remote delivery reduces their stack frames to 64/112 bytes,
  versus 160 bytes for the previous combined delivery function.

## Verification

- Host unit tests exercise the production preallocated heap seam: ordinary
  spare-capacity pushes never invoke the retain predicate, single/batch pressure
  compaction preserves capacity/order, live overflow is fail-stop, and root
  pruning visits only the invalid prefix.
- Static audit every Ready ingress/egress/move updates the exact per-hart count
  under the scheduling-state lock; no other code mutates the count.
- Static cases cover local/remote wake bursts, Ready affinity migration,
  stop/continue, stale root/non-root tokens, full mailbox, drain overflow,
  selection to Running, and a Running task with only stale physical peers.
- RV64 check and clippy with warnings denied, optimized assembly inspection,
  architecture fence, format, unit tests, and `git diff --check`.
- Independent Standards and Spec reviews.

Do not run Make.

## Done criteria

- [x] Ordinary local enqueue and remote delivery never scan their full container.
- [x] Ready count exactly follows every authoritative state transition.
- [x] Stale physical entries cannot affect load selection or tick preemption.
- [x] Capacity pressure remains allocation-free and fail-stop on owner divergence.
- [x] Production heap policy is covered by host unit tests and a static transition audit.
- [x] Architecture/performance contracts, RV64 gates, and independent reviews pass.
