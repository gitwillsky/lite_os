# Plan 005: Allocate one-byte backing for internal notification pipes

> **Executor instructions**: Follow every step and command. Do not run Make,
> commit, or overwrite existing scheduler/VirtIO/documentation WIP. Update the
> status row in `plans/README.md` when done.
>
> **Drift check (run first)**:
> `git diff --stat d4e59a8..HEAD -- kernel/src/ipc.rs kernel/src/task/task_manager/pipe_wait.rs kernel/src/main.rs kernel/src/syscall/epoll.rs kernel/src/syscall/eventfd.rs kernel/src/syscall/socket.rs kernel/src/fs/pty.rs docs/architecture.md docs/architecture-contract.md docs/architecture-interface.txt docs/syscall-support.md`
> Documentation and `main.rs` may already have unrelated WIP. Reconcile it. Stop
> if Pipe constructors or notification call sites no longer match this plan.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `d4e59a8`, 2026-07-15

## Why this matters

Every Pipe currently allocates and zeros a 64 KiB ring, even when it only carries
a coalesced readiness token whose occupancy is zero or one. Each socket or epoll
therefore reserves 64 KiB; each eventfd reserves 128 KiB for two internal tokens.
Keeping the existing Pipe/generation/wait owner but using one-byte backing removes
this deterministic memory waste without creating a second notification system.

## Current state

- `kernel/src/ipc.rs:8,109-139` gives every `Pipe::pair` a 64 KiB vector.
- `ipc.rs:269-300` notification mode accesses only `bytes[0]` and sets length to
  zero or one.
- `task/task_manager/pipe_wait.rs:15-18` exposes one constructor for both data and
  notification pairs.
- Notification callers: DRM/input/PTY assembly in `main.rs:68-76`, epoll
  `syscall/epoll.rs:89`, both eventfd pairs `syscall/eventfd.rs:18-24`, socket
  notification in `syscall/socket.rs:107`, Unix server notification at line 328,
  and inet accept notification at line 376.
- Data callers that must retain 64 KiB: `sys_pipe2`; AF_UNIX socketpair's two
  transport pairs; Unix connect's client/server transport pairs; PTY output.
- `fs/pty.rs:283-309` currently accepts one factory and uses it once for data and
  twice for notifications, so its seam must distinguish the two roles.
- Architecture rule: retain one Pipe lifecycle/generation/notifier owner. Do not
  introduce an independent notification primitive or a dual wait path.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Check | `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel` | exit 0 |
| Lint | `cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings` | exit 0 |
| Architecture | `cargo run --quiet -p architecture-check` | exit 0 |
| Release link | `cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm` | exit 0 |
| Call-site audit | `rg -n "create_(pipe|notification)_endpoints" kernel/src` | every call classified below |
| Diff hygiene | `git diff --check` | no output |

## Scope

**In scope**:

- `kernel/src/ipc.rs`
- `kernel/src/task/task_manager/pipe_wait.rs`
- `kernel/src/task/task_manager.rs` if re-export changes
- `kernel/src/main.rs`
- `kernel/src/syscall/epoll.rs`
- `kernel/src/syscall/eventfd.rs`
- `kernel/src/syscall/socket.rs`
- `kernel/src/fs/pty.rs`
- `docs/architecture.md`
- `docs/architecture-contract.md`
- `docs/architecture-interface.txt`
- `docs/syscall-support.md`
- `plans/README.md`

**Out of scope**: replacing Pipe with a new signal type, changing anonymous or
AF_UNIX capacity/PIPE_BUF semantics, bulk ring-copy optimization, wait-registry
logic, readiness generation semantics, and ABI expansion.

## Git workflow

Use the current branch, no commits/pushes. Before editing dirty docs or `main.rs`,
read their live diff and preserve it hunk-by-hunk.

## Steps

### Step 1: Add one private capacity constructor and two role-specific entries

In `ipc.rs`, factor allocation into a private `pair_with_capacity(notifier,
capacity)` or equivalent. Keep `Pipe::pair` as the 64 KiB data constructor and
add a clearly named notification constructor fixed to capacity 1. Reject zero
capacity structurally; do not store a new kind/mode flag, cache, or duplicate
state owner. Both constructors must retain the same object ID, lock, endpoint
lifecycle, generations, notifier, and OOM behavior.

In `pipe_wait.rs`, keep `create_pipe_endpoints` for data and add
`create_notification_endpoints` for one-byte notification pairs. Document callers
and export the new scoped function only as far as current composition/syscalls
need it.

**Verify**:
`rg -n "PIPE_CAPACITY|notification.*capacity|pair_with_capacity" kernel/src/ipc.rs kernel/src/task/task_manager/pipe_wait.rs`
→ exactly one allocation implementation, data capacity 64 KiB, notification
capacity one.

### Step 2: Classify every constructor call by domain role

Switch only these to `create_notification_endpoints`:

- DRM completion, input-device initialization, and notification half of PTY
  assembly;
- epoll notification and both eventfd readiness directions;
- every socket object's `notify` pair, Unix listening server notification, and
  inet accepted-socket notification.

Keep these on `create_pipe_endpoints`:

- `sys_pipe2`;
- AF_UNIX socketpair `first_to_second` and `second_to_first`;
- Unix connect `client_to_server` and `server_to_client`;
- PTY output transport.

Change `fs::pty::init`/registry to accept distinct data and notification
factories (or one small typed factory bundle), and update the composition root.
Do not infer role from capacity at call sites.

**Verify**: inspect the full output of
`rg -n "create_(pipe|notification)_endpoints" kernel/src`; every match belongs
to exactly one list above and no generic constructor is used for a notification.

### Step 3: Preserve token/generation behavior

Review `signal_readiness`, `drain_readiness`, `poll_state`, close, and notifier
paths with a one-byte vector. No semantic code should need a separate branch:
token occupancy stays 0/1, repeated signal still advances read generation and
wakes, drain still clears without recursive notification, and endpoint close
still publishes EOF/broken state. Do not change wait-registry code.

**Verify**:
`git diff -- kernel/src/ipc.rs kernel/src/task/task_manager/pipe_wait.rs` →
constructor factoring only; notification/generation algorithms are unchanged
except comments or assertions required by the fixed nonzero capacity.

### Step 4: Update scoped interfaces and architecture facts

Document that anonymous/stream data Pipe retains a 64 KiB ring while internal
notification Pipe uses the same owner with one-byte backing. Record the new
task constructor and changed PTY initializer callers in the architecture contract
and regenerate/update the interface baseline honestly. Do not broaden either
function beyond actual callers and do not change fences to suppress a violation.

**Verify**: `cargo run --quiet -p architecture-check` → exit 0.

### Step 5: Run all static gates

Run every command in the table, including the call-site audit and release link.

**Verify**: every command succeeds; `git diff --check` is empty.

## Test plan

The kernel has no host unit-test target and Make is prohibited. Statically trace:
initial empty token; repeated signal coalescing with generation advance; drain and
resignal; reader/writer close; eventfd's independent read/write notifications;
epoll/socket/input/DRM/PTY construction OOM; anonymous pipe full/PIPE_BUF behavior;
AF_UNIX stream short writes; PTY data transport. Data pipes must still expose
65,536-byte capacity, while notification paths can never index beyond byte zero.

## Done criteria

- [x] One allocation implementation accepts a private nonzero capacity.
- [x] Data constructors allocate 64 KiB; notification constructors allocate one
  byte.
- [x] Every constructor call is classified exactly as specified.
- [x] No new notification owner, state flag, cache, lock, or wait path exists.
- [x] Anonymous pipe, AF_UNIX transport, and PTY output remain 64 KiB.
- [x] Interface/docs and all static gates pass; README row says DONE.

## STOP conditions

- Any notification consumer writes or reads arbitrary payload bytes.
- A data transport is ambiguous at a call site or depends on capacity one.
- The PTY seam cannot distinguish output transport from notification without
  leaking a task adapter through filesystem internals.
- Reconciliation would overwrite unrelated dirty documentation/main changes.
- A gate fails twice or an implementation excerpt drifted.

## Maintenance notes

The capacity distinction is a construction-time domain role, not runtime mode
state. Reviewers should reject future notification call sites using the 64 KiB
constructor and future data transports using the one-byte constructor. If a real
notification payload is needed, design a new domain seam rather than silently
growing this token.
