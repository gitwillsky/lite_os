# Plan 015: Bound and accurately report AF_UNIX datagrams

> **Executor instructions**: Follow this plan step by step and run each
> verification before continuing. Stop on any listed STOP condition; do not
> improvise. Update this plan's row in `plans/README.md` when complete unless a
> reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat e891a3f..HEAD -- kernel/src/socket.rs kernel/src/socket/unix.rs kernel/src/syscall/socket.rs kernel/src/syscall/socket/message.rs kernel/src/syscall/fs/io/sequential/write.rs kernel/src/syscall/poll.rs kernel/src/syscall/poll/wait_keys.rs kernel/src/ipc.rs tools/kernel-unit/src/lib.rs user/dynamic-smoke.c docs/architecture-contract.md docs/syscall-support.md`
> Plan 012 is expected to change poll wait-key collection and IPC notification
> draining; Plans 012-014 may all update the architecture documents. They must
> be committed first. Re-read and preserve their final interfaces/content;
> mismatch with the Plan 012 ownership facts below is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: `plans/012-seal-epoll-wait-publication.md`
- **Category**: security
- **Planned at**: commit `e891a3f`, 2026-07-15

## Why this matters

An AF_UNIX datagram socket currently owns an unbounded `VecDeque`. Every send
allocates and enqueues another payload, and the socket remains unconditionally
writable. An unprivileged sender can therefore retain kernel heap until global
OOM; blocking and nonblocking backpressure are unreachable.

The receive path also discards the original datagram length after copying a
short prefix. `recvmsg` then fails to set output `MSG_TRUNC`, and both `recvmsg`
and `recvfrom(MSG_TRUNC)` return the copied count rather than the consumed
message's full length. This plan gives the existing receive-queue owner a fixed
Linux-derived bound, adds race-free peer-capacity waiting, and propagates exact
message length without creating a second queue/counter owner.

## Current state

- `kernel/src/socket/unix.rs:46-60` stores only:

  ```rust
  Datagram {
      messages: VecDeque<Datagram>,
      peer: Option<Weak<UnixSocket>>,
  }
  ```

  `enqueue_datagram` at lines 337-355 allocates/copies a payload and always
  `push_back`s it. There is no message/byte budget or full result.

- `UnixSocket::poll_state` at lines 389-394 reports every datagram socket as
  writable. Its wait source is only its own one-byte notification pipe, so a
  connected sender has no way to wait for its peer receive queue to drain.

- `sys_sendmsg` (`kernel/src/syscall/socket/message.rs:172-184`), `sys_sendto`
  (`kernel/src/syscall/socket.rs:487-499`) and scalar/vector socket write
  (`kernel/src/syscall/fs/io/sequential/write.rs:123-149`) all handle `Again`
  by waiting on the sender OFD. That is correct for streams/INET but cannot wake
  for an unconnected `sendto` destination's capacity.

- `UnixSocket::receive` at `unix.rs:277-284` pops a datagram, copies
  `min(output.len(), message.bytes.len())`, then returns only `(count, source)`.
  `Socket::receive_message` at `socket.rs:373-381` sets `full_length: count`.
  The ABI layer already has correct generic logic: `message.rs:258-272` sets
  output `MSG_TRUNC` when `full_length > count` and returns `full_length` when
  input flags contain `MSG_TRUNC`; `socket.rs:523-540` does the same for
  `recvfrom`.

- Plan 012 establishes `syscall::poll::wait_keys::PollWaitKeys` as the only
  transient concrete-source expansion seam and makes notification draining
  return whether a token was consumed. Reuse that seam; do not construct a
  private wait queue in AF_UNIX.

- The architecture contract makes `UnixSocket::state` the sole connection and
  queue owner, requires Pipe I/O/notification after releasing that lock, and
  makes `SocketWaitSource` the only socket-to-poll adapter
  (`docs/architecture-contract.md:183-184`). Peer-capacity state must remain a
  projection of the target's queue length, not a sender-side shadow counter.

- The fixed Linux v7.1 reference is
  [`net/unix/af_unix.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/unix/af_unix.c).
  It initializes a finite datagram queue length, returns/waits on EAGAIN when a
  peer receive queue is full, relays peer-space wakeups, marks short receives
  with `MSG_TRUNC`, and returns the original skb length for input `MSG_TRUNC`.
  LiteOS need not copy Linux skb machinery, but must preserve these observable
  semantics with its existing indexed Pipe wait seam.
  The same fixed source initializes `sysctl_max_dgram_qlen` to 10 at
  [`af_unix.c:3814`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/unix/af_unix.c#L3811-L3817).

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Queue/ABI policy tests | `cargo test -p kernel-unit` | all tests pass |
| Scheduler regressions | `cargo test -p scheduler-unit` | all tests pass |
| RV64 check | `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel` | exit 0 |
| RV64 lint | `cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings` | exit 0 |
| Architecture | `cargo run --quiet -p architecture-check` | exit 0 |
| Debug kernel for runtime gate | `(cd kernel && cargo build --target riscv64gc-unknown-none-elf --bin kernel)` | exit 0 |
| AF_UNIX runtime regression | `python3 scripts/verify_busybox.py --image target/rootfs.img` | prints BusyBox verification passed or a valid cache hit |
| Release assembly | `(cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm)` | exit 0 |
| Patch hygiene | `git diff --check` | no output |

Never run Make.

## Scope

**In scope**:

- `kernel/src/socket.rs`
- `kernel/src/socket/send.rs`
- `kernel/src/socket/unix.rs`
- one generic queue-policy helper under `kernel/src/socket/unix/`
- `kernel/src/syscall/socket.rs`
- `kernel/src/syscall/socket/message.rs`
- `kernel/src/syscall/fs/io/sequential/write.rs`
- `kernel/src/syscall/poll.rs`
- `kernel/src/syscall/poll/wait_keys.rs`
- `tools/kernel-unit/src/lib.rs`
- `user/dynamic-smoke.c`
- `docs/architecture.md`
- `docs/architecture-contract.md`
- `docs/architecture-interface.txt` (official generator output only)
- `docs/syscall-support.md`
- `plans/README.md`

`kernel/src/ipc.rs` is read-only unless Plan 012's final scoped interface needs
a comment correction; do not change Pipe semantics for this plan.

**Out of scope**:

- Pathname AF_UNIX, SCM_RIGHTS/credentials, MSG_PEEK support, socket-buffer
  sockopts or a new sysctl.
- AF_INET/AF_PACKET queue policy, TCP backpressure or stream staging.
- Removing the existing 65,535-byte global sendmsg/recvmsg stream ceiling; that
  is explicitly deferred in `plans/README.md`.
- A new wait registry, Condvar, task dependency, sender-side capacity counter,
  global, Atomic, cache or persistent flag.
- Changing Plan 012's epoll wait-publication algorithm.

## Git workflow

- Work on `main` only after Plan 012 is reviewed, committed and pushed.
- Inspect the complete dirty tree before handoff and preserve unrelated changes.
- Phased commits and pushes are allowed only after the complete dirty tree and
  this datagram change have been reviewed.

## Steps

### Step 1: Make the receive queue itself the bounded owner

Add a small generic `DatagramQueue<T>` production helper under
`kernel/src/socket/unix/` and use it instead of the raw `VecDeque<Datagram>`.
The queue, not a parallel counter, must own capacity and transitions. Use the
fixed Linux default maximum of 10 queued datagrams unless the pinned v7.1 source
proves a different default. Document that LiteOS currently has no socket-buffer
sysctl, so this is a fixed implementation policy rather than a new UAPI.

Required operations:

- fallible push that returns the unconsumed item as `Full` without changing the
  queue;
- pop that returns the item and whether the transition was full -> non-full;
- `is_empty`, `is_full` and length for projections/tests; and
- bounded allocation failure mapped to the existing `NoMemory` result.

Every AF_UNIX datagram payload is already capped at 65,535 bytes by sendmsg.
Add an AF_UNIX datagram length preflight through the `Socket` facade so
`sendto` and scalar/vector `write` reject larger atomic messages with
`MessageTooLarge` before allocating their full kernel staging buffer. Keep
stream behavior unchanged. A 10-entry queue is then bounded by 10 payloads of
at most 65,535 bytes plus fixed Vec/queue overhead.

**Verify**:

```bash
rg -n "Datagram \{[[:space:]]*$|messages: VecDeque|messages\.push_back" kernel/src/socket/unix.rs
```

Expected: datagram state uses the new bounded owner and no raw push bypasses it.

### Step 2: Return an opaque peer-capacity blocker on full

Do not overload plain `SocketError::Again` with hidden persistent state. Add a
socket-facade send outcome/error shape that distinguishes:

- ordinary WouldBlock, which continues to wait on the sender OFD for streams or
  INET/PACKET; and
- AF_UNIX datagram peer-full, which carries an opaque `SocketSendBlocker` owning
  an Arc to the target socket.

The blocker may expose only facade operations needed by syscall wait
orchestration: its `SocketWaitSource`, an allocation-free `is_ready` level
recheck, and notification preparation. It must not expose `UnixSocket`, queue
length or a concrete adapter across the seam.

When enqueue observes full under the target state lock, return that target
blocker without modifying the queue. Update all three send callers:

- `sys_sendmsg`;
- `sys_sendto`; and
- sequential scalar/vector socket write.

Nonblocking calls return EAGAIN immediately. Blocking calls with an ordinary
WouldBlock retain `wait_for_ofd`. Peer-full calls use the direct blocker wait in
Step 3, then retry the complete send operation. Preserve SIGPIPE, partial stream
write and datagram atomicity behavior.

**Verify**:

```bash
rg -n "SocketError::Again.*wait_for_ofd|Err\(SocketError::Again\)" kernel/src/syscall/socket.rs kernel/src/syscall/socket/message.rs kernel/src/syscall/fs/io/sequential/write.rs
```

Expected: each send caller explicitly distinguishes peer-capacity blocking from
ordinary OFD blocking; no AF_UNIX full result is silently routed only to the
sender OFD.

### Step 3: Publish peer-capacity waits through the indexed registry

Add one narrow `wait_for_socket_send` helper in `syscall::poll` that:

1. appends the blocker's facade-provided source through Plan 012's
   `PollWaitKeys` builder;
2. performs the blocker's notification preparation before publication; and
3. calls the existing task `wait_for_poll` with an allocation-free registry-lock
   closure that returns true when target capacity is available.

On receive pop, release the target state lock before notifying. Signal the
existing target notification Pipe only for full -> non-full (and preserve the
existing readable-token consumption). That source wake either reaches published
membership or is caught by the registry-lock level recheck; no lost wake window
is permitted.

For connected datagram `poll/epoll(POLLOUT)`, project writability from the live
peer queue and include the peer notification source in `wait_sources`. Clone the
Weak peer under the sender state lock, then drop that lock before locking the
target; bidirectionally connected sockets must not create an ABBA lock cycle.
Unconnected datagram poll semantics and explicit-address `sendto` remain
separate: a per-call blocker handles the latter.

Readiness generation must include every source used for requested readiness.
The notification Pipe generations are globally comparable, so use the existing
max projection after all relevant source Arcs are obtained without nested state
locks.

**Verify**:

```bash
rg -n "wait_for_socket_send|SocketSendBlocker|full.*non-full|writable:" kernel/src/socket.rs kernel/src/socket/unix.rs kernel/src/syscall/poll.rs kernel/src/syscall/poll/wait_keys.rs
```

Expected: one facade blocker wait path, one target queue owner, and notification
after the state guard's scope.

### Step 4: Preserve full datagram length through the facade

Before copying a popped datagram, capture `message.bytes.len()`. Return
`(count, full_length, source)` from the AF_UNIX adapter and set
`ReceivedMessage.full_length` to that value in `Socket::receive_message`.
Do not change the existing generic ABI logic in `recvmsg`/`recvfrom` except for
type adaptation.

Required behavior:

- a short receive copies only the buffer capacity and consumes one datagram;
- recvmsg output `msg_flags` includes MSG_TRUNC when `full_length > count`;
- input MSG_TRUNC returns `full_length` for recvmsg and recvfrom;
- without input MSG_TRUNC the syscall returns `count`; and
- zero-length/exact/oversized buffers do not invent a second message-length
  owner.

**Verify**:

```bash
rg -n "full_length: count|Ok\(\(count, message\.source\)\)" kernel/src/socket.rs kernel/src/socket/unix.rs
```

Expected: no AF_UNIX path overwrites full length with copied count.

### Step 5: Add host state-machine and guest ABI regressions

Include the production `DatagramQueue` helper in `tools/kernel-unit` and add
tests for:

- exactly 10 pushes succeed and the next returns Full without losing the item;
- FIFO order is preserved;
- only full -> non-full pop requests a capacity wake;
- empty and maximum-size message entries count as one slot; and
- repeated fill/drain cycles never grow logical capacity or duplicate items.

Extend `user/dynamic-smoke.c::verify_unix_epoll` with self-checking tests that:

- send a six-byte datagram into a two-byte recvmsg iovec and verify copied
  prefix, output MSG_TRUNC and return 6 with input MSG_TRUNC;
- verify the same return-length rule for recvfrom(MSG_TRUNC);
- set the sender nonblocking, send bounded small messages until EAGAIN within a
  conservative upper bound, observe POLLOUT false while full, receive one, then
  observe POLLOUT and a successful send; and
- exercise one blocking full-queue send that wakes after the peer consumes a
  message, using an existing fork/deadline pattern so a lost wake fails rather
  than hanging the whole gate.

Do not assert that queue length 10 is UAPI; the guest test should discover full
within a bound and test transitions.

**Verify**:

```bash
cargo test -p kernel-unit
(cd kernel && cargo build --target riscv64gc-unknown-none-elf --bin kernel)
python3 scripts/verify_busybox.py --image target/rootfs.img
```

Expected: host tests pass and the BusyBox/dynamic smoke gate passes or reports a
valid fingerprint cache hit including the changed probe and kernel.

### Step 6: Update contracts and run all gates

Update `docs/architecture-contract.md` with the fixed queue owner, absence of a
shadow byte counter, the opaque target blocker seam, lock ordering, notification
transition and failure consequence. Update the AF_UNIX row in
`docs/syscall-support.md` to state bounded datagram backpressure and exact
MSG_TRUNC/full-length semantics. Do not claim pathname sockets, sockopts or
SCM_RIGHTS.

Run every command above, inspect the optimized release image for an unbounded
allocation/loop in queue transitions, and request independent Standards and
Spec reviews. Fix every blocker before handoff.

**Verify**: all commands exit 0, `git diff --check` is silent, and both reviews
report no blocker.

## Test plan

- `kernel-unit` tests the exact generic queue owner used in production.
- `dynamic-smoke` verifies ABI-visible truncation, EAGAIN/POLLOUT, and blocking
  wake behavior on the real kernel.
- Existing epoll tests must remain green because peer notification is routed
  through Plan 012's same source-indexed publication seam.
- No elapsed-time performance claim is required. The resource bound is exact:
  at most the fixed number of size-limited payloads per receive queue.

## Done criteria

- [x] Each AF_UNIX datagram receive queue has a fixed, tested message bound.
- [x] sendto/sendmsg/write reject oversized AF_UNIX datagrams before full staging allocation.
- [x] Full nonblocking sends return EAGAIN without queue mutation.
- [x] Full blocking sends wait on target capacity without a lost wake.
- [x] Connected POLLOUT reflects and wakes on live peer capacity without nested socket locks.
- [x] Short recvmsg/recvfrom propagate full length and MSG_TRUNC exactly.
- [x] No sender-side capacity counter, new registry/global/lock/Atomic/cache/flag exists.
- [x] Host, RV64, architecture and release gates pass.
- [ ] Full BusyBox runtime gate passes.
- [x] Independent reviews have no blocker before phased submission.

## STOP conditions

Stop and report if:

- Plan 012 is not committed, or its final `PollWaitKeys`/notification API differs
  enough that a direct target-source wait cannot use the unique registry.
- The fixed Linux v7.1 default datagram queue bound cannot be established; do
  not invent a new UAPI or silently use an arbitrary unbounded byte limit.
- AF_UNIX payload size is not bounded consistently across sendto, sendmsg and
  scalar/vector write before queue retention.
- Waiting requires storing per-send state in `UnixSocket`, adding a task
  dependency to socket, or creating another wait queue.
- Connected peer readiness requires holding two socket state locks at once.
- A popped message must be reinserted after user-copy failure. Existing LiteOS
  destructive-receive/copyout ordering is outside this plan; report the scope
  expansion instead.
- Any gate fails twice after a reasonable correction or files outside Scope are
  required.

## Maintenance notes

- If socket-buffer sockopts are later added, the receive queue remains the only
  capacity owner; replace the fixed bound in that owner rather than adding a
  sender credit counter.
- Reviewers should scrutinize full -> non-full notification after unlock,
  explicit-address sendto blocking, connected-pair lock order and MSG_TRUNC's
  distinction between input return policy and output flag.
- Stream message-size/staging cleanup and pathname AF_UNIX remain deferred.
