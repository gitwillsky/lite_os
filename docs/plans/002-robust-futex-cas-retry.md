# Plan 002: Retry robust-futex owner-death publication after CAS conflicts

> **Executor instructions**: Follow every step and verification gate. Do not run
> Make or create commits. Preserve unrelated working-tree changes and update the
> row in `plans/README.md` when finished.
>
> **Drift check (run first)**:
> `git diff --stat d4e59a8..HEAD -- kernel/src/task/model.rs docs/architecture.md docs/syscall-support.md`
> Reconcile dirty documentation; stop if the robust-list implementation no
> longer matches the excerpt below.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `d4e59a8`, 2026-07-15

## Why this matters

Thread exit currently performs one robust-futex compare-exchange and silently
abandons owner-death publication if a waiter concurrently changes the word. The
dead TID can remain installed without `FUTEX_OWNER_DIED`, permanently blocking a
POSIX robust mutex. Linux retries using the CAS-observed word until ownership
changes or owner death is committed.

## Current state

- `kernel/src/task/model.rs:529-550` copies the word once, checks the TID, then
  reduces nested CAS results to a boolean; the `Err(observed)` value is discarded.
- `kernel/src/memory/mm/user_access.rs:196-214` already returns
  `Result<Result<u32, u32>, UserAccessError>`, so no memory API change is needed.
- `docs/syscall-support.md:70` claims owner-death publication is complete.
- Fixed semantic reference: Linux v7.1 commit
  `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`, robust futex exit cleanup under
  `kernel/futex/`; keep LiteOS's documented 2048-entry traversal bound.

Current failing shape:

```rust
let old = u32::from_ne_bytes(bytes);
let new = old & FUTEX_WAITERS | FUTEX_OWNER_DIED;
let exchanged = memory_set.compare_exchange_user_u32(...)
    .is_ok_and(|result| result.is_ok());
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

**In scope**: `kernel/src/task/model.rs`,
`kernel/src/task/model/robust_list.rs`, `kernel/src/task/model/process_exec.rs`,
`kernel/src/task/pid.rs`, `kernel/src/task/task_manager.rs`,
`kernel/src/task/task_manager/{futex,thread_clone,vfork}.rs`,
`kernel/src/syscall/process.rs`, `docs/architecture.md`,
`docs/architecture-contract.md`, `docs/architecture-interface.txt`,
`docs/syscall-support.md`, `plans/README.md`.

**Out of scope**: futex wait/requeue keys, PI futexes, robust-list traversal
layout/bound, `compare_exchange_user_u32`, and clear-child-tid ordering.

## Git workflow

Stay on the current branch. Do not commit, push, or rewrite pre-existing diffs.

## Steps

### Step 1: Turn `mark` into an observed-value CAS loop

After the initial fault-tolerant user read, keep a mutable `observed`. On every
iteration:

1. stop without waking if `observed & FUTEX_TID_MASK` is no longer this TID;
2. derive `new = observed & FUTEX_WAITERS | FUTEX_OWNER_DIED`;
3. call `compare_exchange_user_u32` under the AddressSpace owner;
4. on user fault, stop the traversal as Linux does; on CAS conflict, assign the
   returned observed value and retry; on success, finish.

Wake one waiter only after successful publication, and only if the replaced
word had `FUTEX_WAITERS`. Do not cap CAS retries independently of ownership: an
arbitrary cap recreates the lost owner-death bug. Do not hold the memory-set lock
while calling `futex_wake`.

For the fixed Linux `list_op_pending` non-PI race, check owner zero on every
iteration and issue the missing wake without manufacturing OWNER_DIED. Skip a
pending node during the main-list walk and process it once afterward, so an
already-linked pending node cannot wake twice.

Preserve the surrounding Linux registration/traversal ABI while moving the
code: a correctly sized NULL head unregisters the list, while a NULL link
encountered within the 2048 processed-node bound terminates cleanup before
pending handling. The fetched next pointer after node 2048 is outside that
bound and does not suppress pending cleanup.
Snapshot the old AddressSpace and user-fault limits once before traversal, and
run the same cleanup during exec after fallible image preparation but before
old-mm replacement. Bound TaskManager's monotonic IDs to FUTEX_TID_MASK so every
published TID remains encodable in the owner field; exhaustion returns EAGAIN
before graph/runqueue publication. Robust wake must resolve its key from the
captured old-mm/limits through the shared queue→memory wake seam.

**Verify**:
`sed -n '20,75p' kernel/src/task/model/robust_list.rs` → shows an explicit
CAS-conflict retry and uses the returned observed word.

### Step 2: Correct the documented completeness proof

Update the robust-list sentence in `docs/syscall-support.md`, the lifecycle
description in `docs/architecture.md`, and the exit concurrency contract in
`docs/architecture-contract.md` to state that CAS conflict rechecks owner TID
and retries before wake. Record the pending zero-owner handoff and duplicate
suppression. Do not claim PI/requeue support or broaden scope.

**Verify**: `rg -n "robust|OWNER_DIED" docs/syscall-support.md docs/architecture.md`
→ the complete claim includes retry/recheck semantics.

### Step 3: Run all static gates

Run every command in the table.

**Verify**: all exit 0 and `git diff --check` has no output.

## Test plan

There is no host kernel unit-test target and Make is prohibited. Statically trace
these cases: immediate CAS success without waiters; success with waiters and one
wake; conflict changing only `FUTEX_WAITERS` then success; conflict transferring
ownership then no write/wake; NULL/unaligned address or user fault at initial
read, in-bound next-link read, or CAS stops traversal; node 2048 may leave an
unchecked next while pending is still handled; correctly sized NULL registration
unregisters; pending owner zero wake without OWNER_DIED; pending already linked
processed exactly once; exec cleans the old mm before replacement; robust wake
uses that old-mm snapshot; allocation at PID_MAX succeeds and the following
allocation returns EAGAIN before publishing an unencodable TID.
Clippy and release-link are regression gates.

## Done criteria

- [x] CAS `Err(observed)` feeds the next iteration.
- [x] Owner TID is checked on every iteration.
- [x] Ordinary wake occurs once, after success, only for `FUTEX_WAITERS`;
  pending owner-zero performs its one compensating wake without a CAS.
- [x] A pending node already present in the main list is processed only once.
- [x] NULL registration unregisters; an in-bound NULL link stops before pending.
- [x] Exec cleans and clears the registration before old-mm replacement.
- [x] Robust wake resolves from the captured old-mm/limits snapshot.
- [x] Every allocated TID is bounded by FUTEX_TID_MASK; exhaustion returns EAGAIN.
- [x] No memory-set guard is held across `futex_wake`.
- [x] All static commands pass; only in-scope files changed.
- [x] `plans/README.md` says DONE.

## STOP conditions

- CAS no longer returns the observed conflicting value.
- Correct retry would require holding MemorySet across wait-registry wake.
- The owner mask/word layout differs from the fixed Linux robust-futex UAPI.
- A gate fails twice or the code excerpt has drifted.

## Maintenance notes

The CAS loop is concurrency protocol, not a best-effort optimization. Future
refactors must retain the retry-on-waiter-bit-race behavior and the lock drop
before wake.
