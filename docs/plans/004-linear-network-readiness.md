# Plan 004: Traverse network readiness maps from their cursor

> **Executor instructions**: Run each step and gate, do not run Make or commit,
> preserve unrelated WIP, and update `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat d4e59a8..HEAD -- kernel/src/socket/inet/readiness.rs`
> Stop if the endpoint loops no longer match the excerpts below.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `d4e59a8`, 2026-07-15

## Why this matters

UDP and raw readiness loops restart at the smallest map key for every endpoint
and filter past the prior cursor. At the 1024-endpoint bound, one snapshot or
transition pass performs 524,800 successful-selection key visits plus a final
1,024-key exhaustion scan, rather than an ordered AVL successor lookup per live
key. `FallibleMap::iter_after` already provides the required allocation-free
cursor and TCP already demonstrates it.

## Current state

- `kernel/src/socket/inet/readiness.rs:17-52` snapshots UDP/raw with
  `iter().filter(handle > cursor).next()` inside a loop.
- Lines 75-114 repeat the pattern for transition capture.
- Lines 137-176 repeat from-start scans while consuming pending UDP/raw edges.
- Lines 53-70, 115-134, and 179-193 use `iter_after` for TCP.
- `kernel/src/fallible_tree.rs:107-150` guarantees ordered, allocation-free
  `iter`, `iter_after`, and `for_each_mut`; no collection change is needed.
- Contract: notification remains outside `NetworkStack` lock and no temporary
  collection may grow with socket count (`docs/architecture.md:128`).

Representative anti-pattern:

```rust
self.endpoints.iter()
    .filter(|(handle, _)| cursor.is_none_or(|cursor| **handle > cursor))
    .map(|(&handle, _)| handle).next()
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

**In scope**: `kernel/src/socket/inet/readiness.rs`, `plans/README.md`.

**Out of scope**: smoltcp polling, readiness semantics, pending-bit ownership,
`FallibleMap`, TCP/packet behavior, lock boundaries, and new caches/indexes.

## Git workflow

Use the current branch without commits or pushes. Do not touch unrelated WIP.

## Steps

### Step 1: Replace restart scans with ordered successors

For each UDP/raw snapshot and capture loop, select the first entry with `iter()`
and subsequent entries with `iter_after(&cursor)`, matching the TCP pattern. A
small private helper local to this file may express `Option<SocketHandle>` cursor
selection if it keeps concrete endpoint state private. Do not allocate a handle
snapshot and do not add a cursor cache.

Preserve the separate get/poll/get_mut phases so Rust borrows do not force unsafe
or lock changes. Preserve stable ascending ID order.

**Verify**:
`rg -n "\.filter\(\|\(handle, _\)\|.*cursor|is_none_or\(\|.*handle" kernel/src/socket/inet/readiness.rs`
→ no from-start cursor filter remains.

### Step 2: Fix pending-edge successor selection

Change `next_pending_udp` and `next_pending_raw` so `after == None` starts with
`iter()` and `Some(handle)` starts with `iter_after(&handle)`. Keep
`notification_pending` clearing under the stack lock and keep each
`endpoint.notify()` outside it in `notify_pending`.

Do not clear a dead weak endpoint's pending bit differently in this plan; retain
existing behavior exactly.

**Verify**:
`rg -n "iter_after" kernel/src/socket/inet/readiness.rs` → UDP, raw, and TCP
successor paths are all present.

### Step 3: Run all static gates

Run every command in the table.

**Verify**: all exit 0 and `git diff --check` emits nothing.

## Test plan

No kernel host test target exists and Make is prohibited. Statically walk empty,
one-entry, sparse-key, and maximum-size maps; pending on first/middle/last key;
dead weak endpoint; no pending edge; and newly captured false→true edges. Confirm
every live key is visited once per logical pass and notify remains lock-free.

## Done criteria

- [x] No UDP/raw cursor loop restarts with `iter().filter(handle > cursor)`.
- [x] All traversals remain allocation-free and in ascending key order.
- [x] Pending bits are consumed once and notify stays outside the stack lock.
- [x] No `unsafe`, global, cache, flag, or interface expansion was introduced.
- [x] All gates pass; only in-scope files changed; README row says DONE.

## STOP conditions

- `SocketHandle` ordering is not compatible with `FallibleMap::iter_after`.
- Borrow resolution appears to require unsafe or changing lock ownership.
- Pending notification order has an external semantic dependency not documented
  in the current architecture.
- The file drifted or a gate fails twice.

## Maintenance notes

Review complexity structurally: every cursor advance must begin at the successor,
not merely hide the old filter in a helper. Future endpoint maps should copy the
ordered TCP/UDP/raw shape.
