# Plan 014: Linearize multi-process signal delivery

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. Stop on any listed STOP condition; do not improvise. When done,
> update this plan's row in `plans/README.md` unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat e891a3f..HEAD -- kernel/src/task/task_manager.rs kernel/src/task/task_manager/signal.rs kernel/src/syscall/signal.rs tools/scheduler-unit/src/lib.rs docs/architecture-contract.md docs/syscall-support.md`
> Plans 012-013 may change the two documents but are not expected to change
> signal ownership. Preserve their content. Any code drift in the symbols below
> is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: `plans/012-seal-epoll-wait-publication.md`
- **Category**: bug
- **Planned at**: commit `e891a3f`, 2026-07-15

## Why this matters

`kill(0, sig)`, `kill(-pgid, sig)` and `kill(-1, sig)` can signal more than one
process. LiteOS currently selects a process, checks permission, increments its
success count, and publishes the signal in three separate process-graph lock
transactions. If the selected process exits between permission and generation,
the syscall returns ESRCH even when an earlier target already received the
signal. Retrying after that false failure repeats an irreversible side effect.

This plan performs selection, permission and signal generation as one graph
transaction per candidate, then releases the graph before notification/wakeup.
The syscall result will reflect actual successful probes/publications while the
process graph remains the unique lifecycle owner.

## Current state

- `kernel/src/task/task_manager/signal.rs:175-218` contains the multi-process
  loop:

  ```rust
  while let Some(tgid) = next_process(selector, cursor) {
      cursor = tgid;
      match process_signal_permitted(sender.as_ref(), tgid, signal) { /* ... */ }
      delivered += 1;
      if signal == 0 { continue; }
      let generated = generate_process_signal(tgid, signal, info)?;
      // publish and wake
  }
  ```

  `next_process` locks the graph at lines 245-261,
  `process_signal_permitted` locks it again at 221-243, and
  `generate_process_signal` takes a third lock at 263-301. The count advances
  before the third transaction succeeds.

- `generate_process_signal` returns `NotFound` if the node has exited or has no
  live thread. The `?` aborts the whole selector loop. `sys_kill` maps that to
  ESRCH in `kernel/src/syscall/signal.rs:40-44`, even if `delivered` already
  reflects another process.

- The thread-directed path at `signal.rs:66-127` is the local exemplar: it
  locates the target, checks permission and queues the signal under one graph
  lock, returns owned wake/notification consequences, and publishes those only
  after unlocking. Match that lock/consequence shape.

- The process graph is the unique owner of live Process membership and process
  group/session identity. The signal pending state remains owned by each
  Process/Thread signal state. Do not add a snapshot Vec or duplicate membership
  registry.

- The fixed Linux v7.1 source is
  [`kernel/signal.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/kernel/signal.c).
  `__kill_pgrp_info` preserves success once any target succeeds, and the tasklist
  read lock prevents a selected target disappearing between selection and
  `group_send_sig_info`. LiteOS uses its process-graph mutex as the equivalent
  lifecycle transaction.

- Do not change `sigaltstack` in this plan. The same fixed Linux source's
  `do_sigaltstack` accepts `SS_ONSTACK` input as well as `0` and `SS_DISABLE`;
  rejecting it was audited and rejected in `plans/README.md`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Signal policy tests | `cargo test -p scheduler-unit` | all tests pass |
| Kernel regressions | `cargo test -p kernel-unit` | all tests pass |
| RV64 check | `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel` | exit 0 |
| RV64 lint | `cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings` | exit 0 |
| Architecture | `cargo run --quiet -p architecture-check` | exit 0 |
| Release assembly | `(cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm)` | exit 0 |
| Patch hygiene | `git diff --check` | no output |

Never run Make.

## Scope

**In scope**:

- `kernel/src/task/task_manager/signal.rs`
- one pure result-fold helper below `kernel/src/task/task_manager/signal/`
- `tools/scheduler-unit/src/lib.rs`
- `docs/architecture-contract.md`
- `docs/architecture-interface.txt` (only through the official generator)
- `docs/syscall-support.md`
- `plans/README.md`

`kernel/src/task/task_manager.rs` and `kernel/src/syscall/signal.rs` may be
changed only if a narrow visibility/comment adjustment is necessary; no syscall
ABI rewrite is expected.

**Out of scope**:

- Thread-directed `tkill/tgkill`, signal frame layout, dispositions, restart,
  realtime queues, capabilities or credential model changes.
- `sigaltstack` flag behavior.
- New graph/signal locks, snapshot collections, generation counters, globals,
  Atomics or a second process-group owner.
- Changing which processes `kill(-1)` excludes.

## Git workflow

- Work on `main` after Plan 012 is committed and pushed.
- Review the complete dirty tree before handoff and preserve unrelated changes.
- Phased commits and pushes are allowed only after the complete dirty tree and
  this signal change have been reviewed.

## Steps

### Step 1: Define the result fold used by production

Add a tiny allocation-free helper below `task_manager/signal/` that is used by
the real selector loop and can be included by `tools/scheduler-unit`. It should
track only the facts needed for Linux return semantics:

- at least one permitted live target was successfully probed (`signal == 0`) or
  had generation completed;
- at least one matching live target was denied; and
- no matching live target was observed.

Once success is recorded, later denials cannot turn it into an error. With no
success, denial wins over NotFound. Invalid signal remains an immediate EINVAL
before iteration and is not folded into candidate results.

Do not store TGIDs or duplicate graph membership in this helper.

**Verify**: `cargo test -p scheduler-unit` -> the new table tests compile and pass.

### Step 2: Make one graph transaction select and generate a candidate

Replace `next_process` + `process_signal_permitted` +
`generate_process_signal` with one internal operation that takes the graph lock
once and, before releasing it:

1. finds the next matching live Process strictly after the cursor;
2. advances the cursor even for a denied candidate;
3. chooses a representative live Thread;
4. evaluates the existing credential/session SIGCONT permission rule using the
   captured sender Arc;
5. records a successful existence probe for signal zero, or queues the
   process-directed signal and computes job-control consequences; and
6. returns only owned, detached consequences: target TGID, optional eligible
   Thread Arc, `queued`, and optional `JobNotification`.

Process exit uses the same graph lock, so a candidate cannot disappear between
steps 1 and 5. Do not hold the graph lock while calling
`publish_job_notification`, `wake_process_signal_waiter`,
`interrupt_waiting_task` or `request_task_reschedule`; retain the established
thread-directed pattern.

Remove the obsolete multi-lock helper functions. Do not keep a compatibility
entry or dual-track generation path.

**Verify**:

```bash
rg -n "fn next_process|fn process_signal_permitted|fn generate_process_signal|delivered \+= 1" kernel/src/task/task_manager/signal.rs
```

Expected: no obsolete helper or pre-generation success increment remains.

### Step 3: Preserve exact selector/error semantics

Wire the production fold into `send_selected_processes` and prove these cases:

- one success followed by any denied/missing candidate returns success;
- only matching denied candidates return Permission/EPERM;
- no matching live candidates return NotFound/ESRCH;
- signal zero counts a permitted live target without queuing/waking;
- ignored/coalesced signals still count as a successful generation attempt;
- positive PID, process-group, caller-group and all-except selectors retain the
  current membership rules; and
- kernel-generated group signals bypass user credential checks exactly as now.

There should be no "stale selected TGID" result after Step 2: exit cannot remove
the node while the graph transaction is active. If a later wake finds no waiter
or the process exits after unlocking, the signal publication already succeeded
and the syscall result remains success.

**Verify**: `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel`
-> exit 0.

### Step 4: Add deterministic policy regressions

In `tools/scheduler-unit`, include the production fold helper and add tests for:

- success then denial => success;
- denial then success => success;
- denial only => Permission;
- no candidate => NotFound;
- signal-zero successful probe => success without a generated consequence; and
- multiple successes count accurately for the kernel process-group caller.

Add a static structural assertion in review (or an architecture-check rule only
if it is stable and narrowly expressible) that the multi-process loop has one
graph-lock acquisition per candidate and performs notification/wakeup after the
guard's scope. Do not add a test-only exit hook or global flag.

**Verify**: `cargo test -p scheduler-unit` -> all existing and new tests pass.

### Step 5: Update the ownership/ABI facts

Update `docs/architecture-contract.md` to state that process selection,
permission and signal generation share one process-graph transaction, while
notification/wakeup is a detached consequence after unlocking. Update the kill
row in `docs/syscall-support.md` only if its completion text needs to record
partial-success semantics; do not change the ABI matrix status or add signals.

**Verify**: `cargo run --quiet -p architecture-check` -> exit 0.

### Step 6: Run all gates and independent review

Run every command above. Inspect the RV64 release assembly/call graph only to
confirm the old three helper calls/lock acquisitions are gone; do not claim a
wall-time speedup. Request independent Standards and Spec reviews and resolve
all blockers before handoff.

**Verify**: every command exits 0, `git diff --check` is silent, and both reviews
have no blocker.

## Test plan

- Production result-fold tests live in `scheduler-unit` and must call the same
  helper as the kernel path.
- The concurrency proof is the shared graph critical section, not a timing
  stress test. A test-only sleep/hook would add state and still be nondeterministic.
- Existing kernel-unit and RV64 gates protect unrelated task/signal behavior.

## Done criteria

- [x] A selected Process cannot exit between permission check and signal generation.
- [x] Success is counted only after a probe or generation actually succeeds.
- [x] Earlier successful delivery can never be overwritten by later ESRCH/EPERM.
- [x] All-denied and empty-selector results remain EPERM and ESRCH respectively.
- [x] Notification, wait interruption and reschedule occur outside the graph lock.
- [x] No snapshot Vec, second membership owner, new lock/global/Atomic/flag exists.
- [x] Production-path tests and every non-Make gate pass.
- [x] Independent Standards and Spec reviews have no blocker.
- [x] The complete dirty tree and this logical change passed review before phased submission.

## STOP conditions

Stop and report if:

- Plan 012 is not committed or signal ownership has drifted from the excerpts.
- `may_signal`, signal queueing or job-control helpers acquire a lock in an order
  that conflicts with an existing graph-lock caller; document the concrete
  cycle rather than moving generation outside the graph.
- Signal generation can allocate/fail with an error other than the already
  validated signal number, and partial publication cannot be distinguished.
- The fix appears to need a TGID snapshot, second process registry or new lock.
- Correct Linux-v7.1 behavior would require changing selector membership or
  credentials, which is outside this plan.
- Any verification fails twice after a reasonable correction.
- Files outside Scope are required.

## Maintenance notes

- Any future multi-target process operation should copy the transaction shape:
  select + authorize + mutate under graph ownership, then publish detached
  consequences after unlock.
- Reviewers should focus on borrow scopes around `continue_process_locked`,
  cursor progress for denied targets, signal-zero behavior and partial-success
  return precedence.
- Realtime queued signal capacity and full capability checks remain explicitly
  deferred; this plan fixes an existing kill lifecycle race only.
