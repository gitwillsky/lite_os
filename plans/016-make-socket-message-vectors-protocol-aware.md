# Plan 016: Make socket message vectors protocol-aware

> **Executor instructions**: Follow this plan in order and run every
> verification command. Stop on a listed STOP condition and report instead of
> improvising. Update this plan's row in `plans/README.md` when complete unless
> a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat e891a3f..HEAD -- kernel/src/syscall/mod.rs kernel/src/syscall/user_iovec.rs kernel/src/syscall/fs/io.rs kernel/src/syscall/fs/io/user_vector.rs kernel/src/syscall/fs/io/sequential/read.rs kernel/src/syscall/fs/io/sequential/write.rs kernel/src/syscall/socket.rs kernel/src/syscall/socket/message.rs kernel/src/socket.rs kernel/src/socket/message_limits.rs kernel/src/socket/unix.rs kernel/src/socket/inet/raw.rs kernel/src/socket/inet/udp.rs kernel/src/socket/packet.rs tools/kernel-unit/src/lib.rs user/dynamic-smoke.c docs/architecture.md docs/architecture-contract.md docs/architecture-interface.txt docs/syscall-support.md plans/README.md`
> Plans 012-015 must already be committed. Plans 013-015 may update the shared
> architecture documents; Plan 015 is expected to change the socket send
> outcome, AF_UNIX message-size preflight, truncation and direct peer-capacity
> wait. Reconcile with their final code/content; replacing or bypassing Plan
> 015's semantics is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: `plans/015-bound-and-report-unix-datagrams.md`
- **Category**: bug
- **Planned at**: commit `e891a3f`, 2026-07-15

## Why this matters

Socket `sendmsg/recvmsg` has a private one-entry-at-a-time iovec importer and
applies a global 65,535-byte total limit before it knows whether the socket is a
datagram or stream. Valid TCP and AF_UNIX stream calls with larger vector totals
therefore fail with EMSGSIZE; even a large receive capacity is rejected although
the peer may return only a few bytes. The duplicate importer also bypasses the
page-batched, checked raw-array seam used by readv/writev.

This plan makes raw Linux/RV64 iovec copyin a single syscall-layer module,
leaves atomic-message limits with the Socket facade, and processes streams with
a bounded cursor/staging buffer. It fixes existing ABI semantics and allocation
shape without adding a syscall, protocol or compatibility entry.

## Current state

- `kernel/src/syscall/socket/message.rs:47-74` contains a second importer:

  ```rust
  if header.iovec_count > MAX_IOVECS ||
      header.iovec_count != 0 && header.iovecs == 0 { /* EINVAL */ }
  for index in 0..header.iovec_count {
      task.copy_from_user(header.iovecs + index * IOVEC_SIZE, &mut bytes)?;
      // ...
      if total > MAX_DATAGRAM_BYTES { return Err(-errno::EMSGSIZE); }
  }
  ```

  The same `read_iovecs` is used by sendmsg and recvmsg for every socket type.
  The 65,535-byte rule is therefore incorrectly treated as an iovec-layout rule.

- `kernel/src/syscall/fs/io/user_vector.rs:26-80` is the established importer.
  It validates count/null/array arithmetic, copies as many entries as fit in one
  userspace page, preserves entry error order, and computes a checked total. Its
  `UserIoVec` and importer are currently `pub(super)` below fs/io, so socket code
  cannot reuse them.

- `message.rs:81-103` gathers the entire sendmsg vector into one Vec before the
  first backend send. `message.rs:246-250` allocates the entire recvmsg capacity
  before one receive. This is required for atomic datagrams but unnecessary and
  hostile to large streams.

- `kernel/src/syscall/fs/io/sequential/write.rs:94-170` and `read.rs:62-99`
  already demonstrate the required stream shape: a `UserIoCursor`, a bounded
  64-KiB kernel buffer, real short/partial progress, and the unique blocking
  wait seam. Reuse a shared cursor implementation; do not call a scalar syscall
  recursively or create a second progress struct.

- Plan 015 gives the `Socket` facade ownership of AF_UNIX atomic message-size
  validation and peer-capacity WouldBlock. Other datagram/raw backends already
  own their protocol errors. The syscall layer may ask the facade to validate an
  atomic send/choose a bounded receive staging size, but may not match on
  `UnixSocket`/`InetSocket` concrete adapters.

- `docs/architecture-contract.md:203` currently calls
  `fs::io::user_vector::import_iovecs` the unique vector import seam, but
  `sendmsg/recvmsg` violate that statement. The corrected owner belongs at
  `syscall::user_iovec`: it handles only RV64 layout/user-copy, while fs and
  socket callers own total-length/error/partial-I/O policy.

- Primary semantics are fixed to Linux v7.1
  [`net/socket.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/socket.c)
  and the protocol implementations at the same commit. Use them to confirm
  stream partial result, datagram atomic limit and errno ordering. Do not use a
  moving kernel branch.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Import/staging tests | `cargo test -p kernel-unit` | all tests pass |
| Scheduler regressions | `cargo test -p scheduler-unit` | all tests pass |
| RV64 check | `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel` | exit 0 |
| RV64 lint | `cargo clippy -p kernel --target riscv64gc-unknown-none-elf --bin kernel -- -D warnings` | exit 0 |
| Architecture | `cargo run --quiet -p architecture-check` | exit 0 |
| Debug runtime image | `(cd kernel && cargo build --target riscv64gc-unknown-none-elf --bin kernel)` | exit 0 |
| Runtime ABI | `python3 scripts/verify_busybox.py --image target/rootfs.img` | verification passed or valid cache hit |
| Release assembly | `(cd kernel && cargo rustc --target riscv64gc-unknown-none-elf --bin kernel --release -- --emit=asm)` | exit 0 |
| Patch hygiene | `git diff --check` | no output |

Never run Make.

## Scope

**In scope**:

- `kernel/src/syscall/mod.rs`
- `kernel/src/syscall/user_iovec.rs` (new unique production raw-iovec module)
- `kernel/src/syscall/fs/io.rs`
- `kernel/src/syscall/fs/io/user_vector.rs` (deleted after moving the unique seam)
- `kernel/src/syscall/fs/io/sequential/read.rs`
- `kernel/src/syscall/fs/io/sequential/write.rs`
- `kernel/src/syscall/socket.rs`
- `kernel/src/syscall/socket/message.rs`
- `kernel/src/socket.rs`
- `kernel/src/socket/message_limits.rs`
- `kernel/src/socket/inet/raw.rs`
- `kernel/src/socket/inet/udp.rs`
- `kernel/src/socket/packet.rs`
- `kernel/src/socket/unix.rs`
- `tools/kernel-unit/src/lib.rs`
- `user/dynamic-smoke.c`
- `docs/architecture.md`
- `docs/architecture-contract.md`
- `docs/architecture-interface.txt` (generated scoped-interface baseline)
- `docs/syscall-support.md`
- `plans/README.md`

The sequential read/write files may only replace their moved cursor import and
duplicate socket-type staging choice with the Socket-facade capacity projection;
their non-socket behavior remains unchanged. `kernel/src/socket/unix.rs` may
receive only the narrow shared-limit adaptation; its Plan-015 queue/backpressure
behavior is otherwise read-only.

**Out of scope**:

- sendmmsg/recvmmsg, ancillary SCM_RIGHTS, pathname AF_UNIX, MSG_WAITALL,
  zero-copy or a new socket option/protocol.
- Changing `readv/writev` SSIZE_MAX/errno/partial behavior.
- Changing Plan 015's fixed AF_UNIX datagram bound, peer blocker or MSG_TRUNC
  semantics.
- Range-sized kernel allocation for streams, a second cursor/progress owner,
  recursive syscall calls, new global/lock/Atomic/cache/flag.

## Git workflow

- Work on `main` after Plans 012 and 015 are committed and pushed.
- Review the complete dirty tree before handoff and preserve unrelated changes.
- Phased commits and pushes are allowed only after the complete dirty tree and
  this vector-I/O change have been reviewed.

## Steps

### Step 1: Move raw iovec copyin to one syscall seam

Create `syscall::user_iovec` (or an equally domain-specific name) and move the
Linux/RV64 `UserIoVec` layout plus page-batched raw array copyin there. Its job
ends after returning entries in userspace order. It must provide checked
`index * 16 + base` arithmetic, copy by page-sized chunks exactly like the
current fs importer, and report structural errors distinctly enough that each
caller preserves its current errno policy.

Keep total-length policy out of the raw copier:

- fs readv/writev computes/enforces its current SSIZE_MAX and EINVAL behavior;
- socket preserves count > 1024 and null-array EINVAL ordering, nonzero null
  element base EFAULT, and maps total overflow/protocol excess as required by
  the fixed socket source.

Keep one shared `UserIoCursor` implementation for scatter/gather progress. Move
it to the new module if both fs and socket need it; otherwise expose it through a
narrow syscall-scoped seam. Update every caller atomically, then delete the old
raw importer. No compatibility wrapper or dual-track importer remains.

**Verify**:

```bash
rg -n "fn (read_iovecs|import_iovecs)|header\.iovecs \+|struct UserIoVec" kernel/src/syscall
```

Expected: one raw ABI layout/import implementation and no unchecked socket
entry-address expression. Policy wrappers may remain but must call that seam.

### Step 2: Make atomic-message limits a Socket-facade decision

After importing the vector list and computing a checked total, ask the Socket
facade whether this send is atomic and whether its protocol accepts the total.
For datagram/raw sends, reject oversize before gathering any payload and keep
the existing MessageTooLarge/EMSGSIZE behavior. For stream sends, do not apply
the 65,535-byte datagram ceiling.

For recvmsg, a large destination capacity is not itself an oversized datagram.
Use a facade-provided maximum useful atomic receive staging length for
datagram/raw sockets, and a fixed 64-KiB staging bound for streams. Scatter only
the actual returned count through the cursor. Preserve Plan 015's AF_UNIX
`full_length` and MSG_TRUNC output/input rules even when total capacity exceeds
the maximum message size.

Do not expose backend variants to syscall code and do not allocate total stream
capacity.

**Verify**:

```bash
rg -n "MAX_DATAGRAM_BYTES|try_reserve_exact\(total_length|resize\(total_length" kernel/src/syscall/socket/message.rs
```

Expected: atomic limits are not in the raw importer and stream paths have no
request-sized allocation. Any constant match is a bounded staging size or a
facade-owned protocol limit.

### Step 3: Stream sendmsg through one cursor with exact progress

For stream sockets, gather at most 64 KiB from the shared cursor, call the
Plan-015 send outcome once per staged chunk, and maintain one completed-byte
count. Required behavior:

- first user-copy failure returns EFAULT; a later failure after sent bytes
  returns the sent partial count;
- a positive short backend send returns total bytes actually sent and does not
  skip the unsent staged suffix;
- ordinary WouldBlock and AF_UNIX peer-capacity WouldBlock use their existing
  unique wait helpers when no progress exists;
- WouldBlock after progress returns the partial count;
- nonblocking first WouldBlock returns EAGAIN;
- BrokenPipe preserves EPIPE and MSG_NOSIGNAL behavior from the fixed Linux
  source/current syscall contract; and
- datagram/raw gather the complete validated message once and submit exactly
  one backend message.

If retaining an unsent staged suffix would require copying cursor state, change
the loop so cursor advancement occurs only for bytes that can be accounted, or
give the existing cursor a single rollback/commit operation. Do not maintain a
second vector index and offset in `message.rs`.

**Verify**: `cargo check -p kernel --target riscv64gc-unknown-none-elf --bin kernel`
-> exit 0.

### Step 4: Bound recvmsg staging without changing message semantics

Allocate only the bounded useful capacity selected in Step 2, perform one
receive operation, then scatter `received.count` through the unique cursor.
Streams may legally return short. Datagram/raw must consume at most one message,
retain full-length/truncation metadata, and never split one message into several
backend receives.

Preserve metadata copyout ordering already documented by the current code. Do
not add a second destructive receive or attempt to restore a consumed datagram
after EFAULT; that broader ordering is out of scope.

**Verify**:

```bash
sed -n '225,290p' kernel/src/syscall/socket/message.rs
```

Expected: bounded allocation, one backend receive, one cursor scatter, then the
existing address/control/msg_flags copyout and return-length policy.

### Step 5: Add production helper and runtime regressions

Include the production raw-iovec layout/chunk helper through `kernel-unit`. Test:

- zero entries with null base;
- count 1024 and rejection of 1025;
- entry arrays aligned, ending at a page boundary and crossing a page boundary;
- checked address multiplication/addition boundaries;
- entry order and total-policy wrappers for zero-length/nonzero-null elements;
- fs SSIZE_MAX behavior remains independent from socket atomic limit; and
- stream staging plans 65,536, 65,537 and multi-megabyte totals into bounded
  chunks without a request-sized allocation.

Extend `user/dynamic-smoke.c` to prove:

- AF_UNIX stream recvmsg with destination capacity >65,535 receives a small
  message instead of EMSGSIZE;
- stream sendmsg whose iovec total is >65,535 makes positive progress (and the
  peer receives exactly that reported prefix), rather than EMSGSIZE;
- AF_UNIX datagram oversize send still returns EMSGSIZE atomically; and
- datagram recvmsg with a capacity >65,535 still reports the Plan-015 exact
  count/full-length/MSG_TRUNC behavior.

Use bounded buffers and fork/poll deadlines so the guest test cannot hang on a
full stream Pipe.

**Verify**:

```bash
cargo test -p kernel-unit
(cd kernel && cargo build --target riscv64gc-unknown-none-elf --bin kernel)
python3 scripts/verify_busybox.py --image target/rootfs.img
```

Expected: tests pass and runtime verification passes or reports a valid changed
fingerprint cache hit.

### Step 6: Update the interface contract and run all gates

Update `docs/architecture-contract.md` so `syscall::user_iovec` owns only raw
iovec layout/import, `UserIoCursor` uniquely owns progress, fs owns SSIZE_MAX,
and Socket owns protocol atomic limits/bounded staging selection. Update the
socket row of `docs/syscall-support.md` to remove the private 65,535-byte stream
restriction while keeping the declared domain/protocol scope unchanged.

Run every command above and request independent Standards and Spec reviews.
Inspect optimized assembly/frame sizes to confirm the staging buffer remains a
fixed bound and no request-sized stack object was introduced.

**Verify**: every command exits 0, `git diff --check` is silent, and both reviews
report no blocker.

## Test plan

- Host tests call the exact raw-layout and chunk-planning helpers used by
  production.
- Guest tests exercise real msghdr/iovec copyin, stream partial progress,
  datagram atomicity and Plan-015 truncation.
- Existing readv/writev kernel and BusyBox tests remain regressions for the
  shared seam.

## Done criteria

- [x] Exactly one checked, page-batched raw RV64 iovec importer exists.
- [x] Socket vector import no longer performs one user-copy per entry.
- [x] Stream sendmsg/recvmsg totals above 65,535 are not rejected as oversized datagrams.
- [x] Stream staging is fixed-bounded and reports exact EFAULT/short/block partial progress.
- [x] Datagram/raw size rejection remains pre-gather and atomic.
- [x] Large datagram receive capacity remains legal and preserves full_length/MSG_TRUNC.
- [x] No duplicate cursor, recursive syscall, request-sized allocation, new global/lock/Atomic/cache/flag exists.
- [ ] Host, RV64, architecture, runtime and release gates pass.
- [x] Independent reviews have no blocker before phased submission.

## STOP conditions

Stop and report if:

- Plan 015 is not committed or its send/blocker/truncation seam cannot be
  preserved.
- Fixed Linux v7.1 errno/partial semantics for oversized iovec totals or
  MSG_NOSIGNAL cannot be established; do not guess.
- A stream short send can leave an unsent staged suffix that cannot be retained
  without duplicating cursor progress. Redesign the cursor transaction first or
  report the scope issue.
- Correct recvmsg behavior requires implementing MSG_WAITALL or restoring a
  consumed datagram after copyout failure.
- Sharing the importer would expose fs/socket concrete adapters across syscall
  seams rather than a production-neutral ABI value.
- Any gate fails twice after a reasonable correction or files outside Scope are
  required.

## Maintenance notes

- New vector syscalls must import raw arrays through `syscall::user_iovec`; each
  subsystem then applies its own semantic total/progress policy.
- Reviewers should scrutinize cursor advancement on short send, first versus
  later EFAULT, nonblocking/MSG_NOSIGNAL behavior and the separation between
  receive capacity and atomic message size.
- sendmmsg/recvmmsg and zero-copy are intentionally deferred; they must reuse the
  same importer/cursor rather than introduce a new path.
