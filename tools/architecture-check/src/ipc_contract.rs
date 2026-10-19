//! System V 实际对象、SEM_UNDO membership 与 timed event 唯一 owner 围栏。
use super::SourceFile;
fn source<'a>(sources: &'a [SourceFile], path: &str, errors: &mut Vec<String>) -> Option<&'a str> {
    let found = sources.iter().find(|source| source.relative == path);
    if found.is_none() {
        errors.push(format!("ipc-owner: missing actual source {path}"));
    }
    found.map(|source| source.text.as_str())
}
fn require(
    sources: &[SourceFile],
    path: &str,
    clauses: &[&str],
    error: &str,
    errors: &mut Vec<String>,
) {
    if let Some(text) = source(sources, path, errors) {
        for clause in clauses {
            // rustfmt 把长调用拆成多行时会补尾逗号；比较前去掉空白和 `,)`，使规则不依赖排版。
            if !text
                .split_whitespace()
                .collect::<String>()
                .replace(",)", ")")
                .contains(&clause.split_whitespace().collect::<String>())
            {
                errors.push(format!("ipc-owner: {path}: {error}; missing {clause}"));
            }
        }
    }
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    require(
        sources,
        "kernel/src/socket/unix/stream/write.rs",
        &[
            "sender.reserve_stream(",
            "message.prepare_attach(",
            "Box::<[u8]>::try_new_uninit_slice(capacity)",
            "bytes.write_copy_of_slice(&input[..capacity])",
            "state.messages.push(prepared)",
            "core::mem::size_of::<Option<UnixRights>>()",
            "Box::<Option<UnixRights>>::try_new_uninit()",
            "slot.write(rights.take())",
            "drop(state)",
            "drop(retired_preparation)",
            "if state.write_disabled",
            "if input.is_empty()",
            "if state.read_closed",
        ],
        "stream must prepare actual barrier and graph before publication, then retire outside direction guard",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/packet_send.rs",
        &[
            "VacantEntry::<u64, Datagram>::allocation_size()",
            "CapacityKind::SendMemory",
        ],
        "packet admission must charge actual node storage and wait on sender allocation watermark",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/send_memory.rs",
        &[
            "buffers.used >= buffers.send_limit",
            "buffers.send_limit / 4",
            "impl Drop for SenderCharge",
            "drop(buffers);",
        ],
        "packet admission and POLLOUT must derive from one actual sender memory ledger with retirement outside transport guards",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/receive.rs",
        &[
            "output.prepared_remaining() >= bytes_needed",
            "output.prepare_append(bytes_needed)",
            "message.source.sender()",
        ],
        "packet staging must prepare actual front bytes outside the endpoint guard and retain sender charge on peek",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/retirement.rs",
        &[
            "self.is_backlog_owned()",
            "self.peer_retirement(&retired, embryo)",
            "address.path_owner.take()",
            "drop(path_owner)",
            "peer.oriented_peer_closed(self.node_id(), *reset)",
        ],
        "actual OFD retirement must report backlog/unread reset and release inode pins outside guards",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/stream.rs",
        &[
            "read_closed: bool",
            "state.read_closed = true",
            "state.read_closed || state.writer_closed",
        ],
        "stream SHUT_RD must stop future admission and preserve its actual byte/frame queue for drain",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/datagram_disconnect.rs",
        &[
            "peer.take()",
            "messages.take_all()",
            "Arc::ptr_eq(peer, target)",
            "Some(SocketError::ConnectionReset)",
            "drop(messages)",
        ],
        "datagram disconnect must atomically retire the actual peer queue without clearing a replacement connection",
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/credentials.rs",
        &[
            "fn message_identity(&self)",
            "self.namespace_authority(), self.resuids(), self.resgids()",
        ],
        "message credentials must freeze IDs and authority from the same actual owner",
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/message_identity.rs",
        &[
            "self.authority.capable(pid_owner, Capability::SYS_ADMIN)",
            "self.uids.contains(&uid)",
            "self.gids.contains(&gid)",
            "Capability::SETUID",
            "Capability::SETGID",
        ],
        "SCM credential claims must use native three-ID and namespace-scoped capability rules",
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/socket/credentials.rs",
        &[
            "bytes.len() != 12",
            "let ids = task\n        .message_identity()\n        .validate_claim(",
            "namespace.lookup_identity(pid)",
            "-errno::ESRCH",
        ],
        "native ucred preparation must use actual namespace PID identity rather than a live-task selector",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/stream.rs",
        &[
            "same_credentials(credentials, frame.credentials.as_ref())",
            "mode.is_message()",
            "state.plan_read(output.remaining(), pass, mode.is_message())",
            "rights.take()",
            "drop(state);\n        drop(retired);",
            "frame.consumed == frame.bytes.len()",
            "front_mut()",
            "let credentials = plan.credentials;",
        ],
        "actual stream frames must preserve credential boundaries, zero-iterator message controls and allocation-free adjacent chunk reads",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/credentials.rs",
        &[
            "Arc::ptr_eq(&self.pid, &other.pid)",
            "self.claimed.as_ref()",
        ],
        "message credentials must pin actual historical PID and keep explicit claims independent of policy",
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/socket/control.rs",
        &[
            "credentials.claimed = Some(super::credentials::prepare(task, message.data)?)",
            "control_length.saturating_sub(written)",
            "reservation.publish(file)",
            "SCM_CREDENTIALS,",
            "super::credentials::encode(task, credentials.as_ref())",
        ],
        "native credentials precede rights; FD copyout success publishes each prefix before subsequent metadata",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/rights_owner.rs",
        &["RightsOwner::Creator | RightsOwner::Backlog(_)"],
        "automatic credentials must follow actual unopened/backlog ownership rather than a second accepted flag",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/lifecycle.rs",
        &[".set_pass_credentials(self.listener.pass_credentials())"],
        "accepted UNIX child must inherit listener SCM policy at actual connect publication",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/capacity.rs",
        &[
            "CapacityKind::PacketQueue",
            "CapacityKind::Backlog",
            "backlog.capacity_available()",
            "_ => true",
        ],
        "socket capacity wait must recheck actual packet/backlog owner and let terminal state report errors",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/connection_backlog.rs",
        &["limit.saturating_add(1)", "self.capacity_available()"],
        "UNIX backlog admission must include native plus-one and unpublished reservations in one ledger",
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/socket.rs",
        &[
            "SocketConnectError::BacklogFull",
            "wait_for_socket_capacity(&capacity, deadline)",
            "let nonblocking = ofd.status_flags() & O_NONBLOCK != 0",
        ],
        "blocking UNIX connect must wait on actual listener capacity with frozen invocation policy",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/timeouts.rs",
        &[
            "struct SocketTimeouts(Mutex<TimeoutValues>)",
            "SocketTimeout::Infinite",
            "crate::timer::SCHEDULER_TICK_NS",
            "now_ns.saturating_add(duration)",
        ],
        "socket timeout must use one actual OFD policy and a saturating monotonic deadline",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/tcp.rs",
        &[
            "struct TcpHandle",
            "passive: Option<PassivePolicy>",
            "timeouts: Arc<SocketTimeouts>",
            "if entry.passive.is_none()",
        ],
        "TCP must own passive policy in the actual protocol handle and avoid rewriting established pending children",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/tcp/io.rs",
        &[
            "State::Established | State::CloseWait if entry.passive.is_none()",
            "timeouts: state.timeouts.snapshot()",
            "options: state.options",
            "policy.connection.observe(tcp, reset)",
            "policy.pending_error = Some(error)",
            "continue",
        ],
        "TCP child policy must freeze on the final ACK and retain established reset children in the actual accept queue",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/tcp/accept.rs",
        &[
            "entry.passive.take()",
            "timeouts.initialize(policy.timeouts)",
            "let options = policy.options",
            "timeouts: timeouts.clone()",
        ],
        "accept must move the actual pending policy rather than copy current listener values",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/poll.rs",
        &[
            "PollIngressSingleResult::SocketStateChanged",
            "take_ingress_reset()",
            "tcp::maintain(self, reset)",
            "tcp::maintain(self, None)",
        ],
        "TCP must observe each actual ingress transition before the next wire frame and observe egress termination",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/tcp.rs",
        &[
            "struct TcpConnection",
            "self.terminal",
            "self.terminal = true",
            "reset.matches(self.local, self.peer)",
            "Ok(connection.local)",
            "State::Closed | State::SynSent",
        ],
        "TCP connection identity must survive routing tuple deletion and consume only one actual terminal event",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/tcp/io.rs",
        &[
            "state.pending_error.take()",
            "else if connection.peer_closed",
            "else if let Some(error)",
            "connection.shutdown_read || !tcp.may_recv()",
        ],
        "TCP I/O must consume errors once, drain real bytes before shutdown, and preserve FIN EOF priority",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/tcp/io.rs",
        &[
            "connection.shutdown_read = *shutdown_read",
            "connection.shutdown_write = *shutdown_write",
            "connection.observe(tcp, reset)",
            "hangup: (shutdown_read && shutdown_write) || state == State::Closed",
        ],
        "SYN_RECV shutdown permissions must move only at the actual final ACK and defer FIN",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/tcp/lifecycle.rs",
        &[
            "fn prepare_closed_transport(",
            "fn disconnect_locked(",
            "network.queue_tcp_resets(super::super::now())",
            "entry.passive = None",
            "let mut retained = false",
            "*tcp = placeholder_socket()",
        ],
        "TCP disconnect must preserve one actual facade slot and retire reset tuples into the sole budgeted packet owner before reuse",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/inet/tcp/io.rs",
        &["tcp.state() != State::Closed || tcp.remote_endpoint().is_some()"],
        "TCP orphan retirement must retain actual FIN/TIME_WAIT lifetime, while reset payloads hold no transport identity",
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/socket/timeout.rs",
        &[
            "!(0..1_000_000).contains(&micros)",
            "-errno::EDOM",
            "SocketTimeout::finite(0)",
        ],
        "socket timeout codec must preserve native invalid-usec and negative-second semantics",
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/poll.rs",
        &["wait_for_poll(keys, deadline,"],
        "socket wait must publish the caller-frozen deadline in the actual registry",
        errors,
    );
    for path in [
        "kernel/src/syscall/socket/message.rs",
        "kernel/src/syscall/fs/io/sequential/read.rs",
    ] {
        require(
            sources,
            path,
            &[
                "deadline",
                "interrupted_wait(",
                "WaitResult::TimedOut => return -errno::EAGAIN",
            ],
            "socket I/O must honor frozen timeout and interruption policy",
            errors,
        );
    }
    require(
        sources,
        "kernel/src/syscall/fs/io/sequential/write.rs",
        &[
            "crate::syscall::socket::write_vectors(task, ofd, socket, vectors, total_length, nonblocking)",
        ],
        "sequential socket write must delegate to the sole send engine owning frozen timeout and progress",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/namespace/description.rs",
        &[
            "cursor >= super::super::ids::IPC_ID_SLOTS",
            "registry.ids.at_or_after(first)?",
            "queue.description()",
            "set.description()",
            "segment.description()",
        ],
        "sysvipc must iterate actual ordered registry slots without a copied statistics ledger",
        errors,
    );
    require(
        sources,
        "kernel/src/fs/procfs/opened_operations.rs",
        &[
            "ProcOpenFile::IpcTable",
            "target: inode.source.current_ipc_namespace()?",
            "pid: inode.source.current_pid_namespace()",
            ".read(output, offset, target, *table, viewer, pid)",
        ],
        "sysvipc open must pin actual IPC/PID namespace and immutable seq user view",
        errors,
    );
    require(
        sources,
        "kernel/src/fs/procfs/ipc_description.rs",
        &[
            "const RECORD_BYTES: usize = 512",
            "bytes: [u8; RECORD_BYTES]",
            "namespace.next_description(table, cursor)",
            "if self.consumed < self.length",
            "if offset != self.position",
            "self.consume(copied)",
            "format_row(&mut output, row, user, pid)",
        ],
        "sysvipc sequential reads must retain one fixed record and actual slot cursor without whole-table rerendering",
        errors,
    );
    if let Some(text) = source(sources, "kernel/src/fs/procfs/ipc_description.rs", errors)
        && (text.contains("Vec<") || text.contains("ProcText") || text.contains("try_reserve"))
    {
        errors.push(
            "ipc-owner: sysvipc codec must not allocate a table-sized snapshot or output buffer"
                .into(),
        );
    }
    require(
        sources,
        "kernel/src/ipc/namespace/shared_memory.rs",
        &[
            "PreparedCreator::new()?",
            "pending.take().expect(\"unpublished shm preparation\")",
            "registry.ids.withdraw_key(id)?",
            "segment.mark_destroy()",
            "segment.reserve_attach(access, requested)?",
            "segment.close(actor, registry.forced_remove)",
            "Some(segment.destroy())",
            "drop(retirement)",
            "registry
                    .ids
                    .iter_from(cursor as u16)",
            "segment.may_destroy_orphan(true)",
        ],
        "shm namespace must own actual admission, key-only RMID, attachment and guarded retirement",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/shared_memory.rs",
        &[
            "state.permission.mode |= SHM_DEST",
            "state.permission.key = 0",
            "state.attaches == 0",
            "state.attaches.checked_add(1)",
            "checked_sub(1)",
            "state.attach_time = now",
            "state.detach_time = now",
            "state.last_operator.replace(pid)",
            "state.backing.take()",
            "self.creator_membership.detach()",
            "self.namespace.shared_memory_close(&self.segment, None)",
        ],
        "shm segment must count actual VMA/reservation members and retire backing independently of observers",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/namespace/shared_memory.rs",
        &[
            "let limit = read_limit()",
            "segment.require_lock(access, locking, limit)?",
            "limit / PAGE_SIZE as u64",
            "crate::namespace::RlimitCharge::acquire(",
            "real_uid,",
            "Capability::IPC_LOCK",
            "segment.publish_lock(&backing, locking, &mut prepared_charge, access, limit)?",
            "policy.set_locked(locking)",
            "drop(policy)",
        ],
        "SHM_LOCK must use actual current limit, real UID charge and one backing admission policy",
        errors,
    );
    require(
        sources,
        "kernel/src/namespace/rlimit_counts.rs",
        &[
            "current_uid = parent_uid",
            "maximum = namespace.rlimit_counts.parent_maxima.maximum(resource)",
            "self.chain[..self.charged]",
            ".decrement(self.resource, *uid, self.amount)",
            ".rlimit_counts",
            "users.take_entry(&key)",
        ],
        "locked pages must follow actual UserNS ancestor owners and return original successful charges",
        errors,
    );
    require(
        sources,
        "kernel/src/memory/swappable_page.rs",
        &[
            "Some(owner) => match owner.try_shared_swap()",
            "let _shared_swap",
        ],
        "every shared-page swap entry must honor the actual backing unevictable gate",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/shared_memory_creator.rs",
        &[
            "state: Mutex<Option<FallibleMap<CreatorKey, CreatorRegistration>>>",
            "self.state.lock().take()",
            "namespace: Weak<IpcNamespace>",
            "segment: Weak<SharedMemorySegment>",
            "namespace.shared_memory_reap_orphan(&segment)",
            "entries.take_entry(&self.key)",
        ],
        "shm creator exit must consume actual clist membership without retaining Task/PID or namespace cycles",
        errors,
    );
    require(
        sources,
        "kernel/src/memory/shared_memory.rs",
        &[
            "source.lifetime.open()",
            "self.lifetime.open()",
            "self.lifetime.close()",
            "may_write: source.may_write",
            "impl Clone for SharedMemoryVmaMembership",
        ],
        "shm VMA membership must call actual lifecycle once for each retained VMA",
        errors,
    );
    require(
        sources,
        "kernel/src/memory/mm/shared_area.rs",
        &[
            "(original.start < start).then",
            "(end < original.end).then",
            "sysv: shared.sysv",
            "self.statistics.snapshot()",
        ],
        "shm partition must transfer middle and open only actual nonempty fragments",
        errors,
    );
    require(
        sources,
        "kernel/src/memory/swappable_page.rs",
        &[
            "shared_statistics: Option<Arc<SharedPageStatistics>>",
            "statistics.admit(state.backing.residency())",
            "statistics.transition(self.backing.residency(), next.residency())",
            "statistics.retire(self.backing.residency())",
            "state.replace_backing(Backing::Swapped(slot))",
            "state.replace_backing(Backing::Discarded)",
        ],
        "shared page aggregate must follow the canonical physical backing transaction and actual membership retirement",
        errors,
    );
    if let Some(text) = source(sources, "kernel/src/memory/mm/shared_area.rs", errors) {
        let start = text.find("frame.admit_shared_statistics");
        let publication = text.find("pages.frames.commit_vacant(prepared)");
        if start
            .zip(publication)
            .is_none_or(|(start, publication)| start >= publication)
            || text.contains("page.shared_memory_statistics()")
        {
            errors.push("ipc-owner: shared page statistics must admit only the winning indexed page before infallible publication, without a per-page query walk".into());
        }
    }
    require(
        sources,
        "kernel/src/task/model/shared_memory.rs",
        &[
            "crate::task::current_tgid_identity()",
            "retirement.synchronize()",
            "memory.prepare_shared_memory_detach(address)?",
            "creator.retire()",
            "self.install_user_mapping(crate::memory::UserMappingRequest",
            "crate::memory::UserMappingSource::SystemV(source)",
        ],
        "shm VMA actor must be current executor and final close must follow guard-free fence",
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/process_exit.rs",
        &["self.retire_shared_memory_creator()"],
        "real Thread exit must retire shm creator before namespace leave",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/namespace.rs",
        &[
            "UserResource::Ipc",
            "messages: Mutex<MessageRegistry>",
            "semaphores: Mutex<SemaphoreRegistry>",
            "ids: IpcIds<MessageQueue>",
            "ids: IpcIds<SemaphoreSet>",
            "PreparedIpcObject::new(key)?",
            "PreparedSemaphores::new(count as usize)?",
            "registry.used += count as usize",
            ".checked_sub(count)",
            "queue.destroy()",
            "set.destroy()",
        ],
        "namespace must own actual registries, creator charge, admission ledger and final content retirement",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/ids.rs",
        &[
            "Arc<MaybeUninit<T>>",
            "NodeSlot<u16, Arc<T>>",
            "self.get(id)?.key()",
            "object.id() == id",
            "sequence = (sequence + 1) & u32::from(u16::MAX)",
            "prepared.storage.assume_init()",
        ],
        "one preallocated ID/key publication and standard IPC64 sequence owner",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/permissions.rs",
        &[
            "access.filesystem_gid == gid",
            "let uid = access.authority.effective_uid",
            "access.authority.capable(user, Capability::IPC_OWNER)",
            "access.authority.capable(user, Capability::SYS_ADMIN)",
        ],
        "DAC must use EUID, fsgid/groups and actual target UserNS capabilities",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/semaphore.rs",
        &[
            "operation.changes[..position].iter().rev()",
            "AtomicAttempt::Blocked(position)",
            "!(-SEM_VALUE_MAX - 1..=SEM_VALUE_MAX).contains(&next_undo)",
            "drop(adjustments)",
            "operation.retired_pids.push(old)",
            "self.pending.state.lock().complete.take()",
            "state.operation_time = now",
            "(semaphore.value + adjustment).clamp(0, SEM_VALUE_MAX)",
            "super::sem_undo::retire_registrations(&mut state.undo)",
        ],
        "array must own atomic rollback, completion/cancel, undo clamp and removal",
        errors,
    );
    require(
        sources,
        "kernel/src/ipc/sem_undo.rs",
        &[
            "members: usize",
            "set: Weak<SemaphoreSet>",
            "list: Weak<SemaphoreUndoList>",
            "if !share",
            "(state.members == 0).then",
            "set.apply_undo(entry, pid, now)",
            "let mut array = set.state.lock()",
            "let mut list = self.state.lock()",
            "node.value_mut().retired_list_node = list_node",
            "node.value_mut().retired_list = Some(list)",
        ],
        "only actual Thread members trigger undo; both indices and retirement must reference one actual entry",
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/process_exit.rs",
        &["retire_semaphore_undo()"],
        "actual exit must retire membership before namespace leaves",
        errors,
    );
    require(
        sources,
        "kernel/src/task/process_table/task_wait.rs",
        &[
            "ticket.prepare_task_event(key, self.clone())",
            "super::arm_indexed_wait(",
            "WaitMembership::TaskWait(key)",
            "super::WAIT_REGISTRY.wake_task_event(key, result)",
            "Arc::ptr_eq(task, &claimed.task)",
        ],
        "timed event source/signal/expiry must consume one actual registry membership",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/namespace.rs",
        &[
            "let mut prepared =",
            "let mut bound = socket.address.lock()",
            "namespace.take_entry(&key)",
            "BindingRequest::Automatic",
            "prepared.set_key(key)",
            "NamespaceKey::Abstract(net_id, address, socket_type)",
            "matches!(*state, SocketState::Closed)",
            "drop(state)",
            "drop(namespace)",
            "drop(bound)",
            "drop(retired)",
        ],
        "UNIX bind must publish endpoint/binding once and retire exact stale node outside guards",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/rights_graph.rs",
        &[
            "edges: FallibleMap<EdgeKey, GraphEdge>",
            "PreparedAttach",
            "AttachError::NeedsPreparation",
            "state.edges.take_entry(&(batch_id, index))",
            "Arc::ptr_eq(node, successor)",
            "self.roots.push(owner)",
            "self.probes.push(socket)",
            "let mut scratch = state.take_scratch()",
            "COLLECTOR.lock_prepared(&mut gate)",
            "COLLECTION_REQUEST.wait_idle()",
        ],
        "SCM graph must prepare tokens, retire exact edges and root probe pins outside GRAPH and preserve actual worker",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/rights_graph.rs",
        &[
            "file_reference: uid.is_some()",
            "edge.file_reference",
            "BacklogOwnership",
            "graph().lock().attach(\n                None,",
            "graph().lock().detach(\n            None,",
            "replace_description(owner)",
        ],
        "backlog edges must express actual listener ownership without UID/OFD reference accounting",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/rights_graph.rs",
        &[
            "pub(super) fn pin_description",
            "let _collector = COLLECTOR.lock()",
        ],
        "historical listener Weak capture must serialize with collector revoke",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/stream.rs",
        &[
            "pub(super) fn close(&self)",
            "state.read_closed = true",
            "core::mem::take(&mut state.messages)",
            "drop(messages)",
        ],
        "stream close must retire actual chunks and controls despite observer Arc",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/stream.rs",
        &[
            "if peek {",
            "for frame in state.messages.iter()",
            "output.prepare_append(plan.capacity)",
            "rights.copy_for_receive(&mut copies)",
            "rights.map(UnixRights::into_received)",
        ],
        "MSG_PEEK must copy real OFD roots with storage prepared outside direction guard and leave original barriers/bytes/inflight ownership intact",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/rights.rs",
        &[
            "pub(crate) struct UnixReceivedRights(Vec<Arc<dyn UnixPassedFile>>)",
            "prepared.capacity() >= self.files.len()",
            "prepared.extend(self.files.iter().cloned())",
            "UnixReceivedRights(self.into_files())",
        ],
        "receive capabilities are actual syscall roots; peek copies must not attach or refund original UID graph",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/stream.rs",
        &[
            "if peek {",
            "frame.consumed += count",
            "frame.consumed == frame.bytes.len()",
            "drop(state);\n        drop(retired);",
        ],
        "one actual chunk owner serves consuming reads and nonmutating peeks; only completed allocations refund sender memory",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/rights_owner.rs",
        &[
            "Backlog(super::rights_graph::BacklogOwnership)",
            "RightsOwner::Retired",
            "owner.upgrade()",
            "retire_rights_owner",
            "super::rights_graph::publish_description(self, owner)",
        ],
        "creator/backlog/OFD/retired roots must have actual lifecycle capabilities",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/retirement.rs",
        &[
            "core::mem::replace(&mut *state, SocketState::Closed)",
            "self.retire_rights_owner()",
            "server.close_description()",
            "backlog.take_pending()",
            "drop(state)",
            "drop(retired)",
        ],
        "actual close/GC must retire pending children and controls outside endpoint guards",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/rights.rs",
        &[
            "limit: Option<Arc<dyn UnixFileLimit>>",
            "self.limit.as_ref().map(|limit| limit.max_files())",
        ],
        "SCM commit must read actual caller limits owner instead of frozen syscall quota",
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/resource_limits.rs",
        &[
            "self.process().resource_limits.clone()",
            "mod unix_file_limit",
        ],
        "limit projection must pin only the actual scalar limits allocation, not Task/Process membership",
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/resource_limits/unix_file_limit.rs",
        &[
            "impl crate::socket::UnixFileLimit for spin::Mutex<ResourceLimits>",
            "self.lock().values[RLIMIT_NOFILE].soft",
        ],
        "limit getter must read and clamp the actual current RLIMIT_NOFILE owner",
        errors,
    );
    require(
        sources,
        "kernel/src/file/unix_passed.rs",
        &[
            "let rooted = Arc::strong_count(&self) > inflight.saturating_add(1)",
            "(rooted, self)",
        ],
        "one actual OFD root probe must return its pin without dropping it under GRAPH",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/stream.rs",
        &[
            "messages: FrameQueue",
            "retired.push(state.messages.pop()",
            "drop(state)",
        ],
        "stream must prepare actual barrier and graph before publication, then retire outside direction guard",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/stream/queue.rs",
        &[
            "self.head = frame.next.take();",
            "self.tail = None;",
            "while let Some(frame) = self.pop()",
            "frame.next.is_none()",
        ],
        "stream FIFO must transfer actual nodes, clear retired tail and drop outside guard without recursion",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/datagram_queue.rs",
        &[
            "PreparedDatagram<T>(VacantEntry<u64, T>)",
            "FallibleMap::try_reserve_node()",
            "self.entries.commit_vacant(prepared.0)",
        ],
        "datagram must use actual preallocated admission and retirement node",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/packet_send.rs",
        &[
            "message.prepare_attach(self)?",
            "source: charge",
            "prepared.value_mut().rights = rights.take()",
            "shutdown.read",
        ],
        "packet byte/control publication must use actual sender identity, native shutdown and guard-external graph preparation",
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/socket/control_list.rs",
        &[
            "ControlSnapshot",
            "length > i32::MAX as usize",
            "task.copy_from_user_uninit(pointer, bytes.prepare(length))",
            "unsafe { bytes.publish(length) }",
            "pub(super) struct ControlList",
            "next.min(self.bytes.len())",
        ],
        "sendmsg ancillary import must freeze one initialized snapshot and preserve native next-header tail semantics",
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/socket/control.rs",
        &[
            "snapshot: &ControlSnapshot",
            "ControlList::new(snapshot.bytes())",
            "message.data.as_chunks::<4>().0",
        ],
        "SCM parsing must consume frozen control data and complete ints rather than rereading userspace",
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/socket/message.rs",
        &[
            "ControlSnapshot::import(",
            "socket.check_seqpacket_send()",
            "stream_send_wait(",
            "cursor.advance(sent)",
        ],
        "raw name/control copy must precede native SeqPacket preflight and frozen SCM parsing, with one stream progress owner",
        errors,
    );
    require(
        sources,
        "kernel/src/socket/unix/lifecycle.rs",
        &[
            "connectable_shutdown(&client_state",
            "backlog.rollback(reservation)",
            "self.listener.notify()",
            "core::mem::replace(&mut *client_state, prepared)",
        ],
        "concurrent connect must decide only at actual publication and notify rolled-back capacity",
        errors,
    );
    if let Some(text) = source(sources, "kernel/src/socket/unix.rs", errors)
        && text.contains("    Connecting,")
    {
        errors.push("ipc-owner: unpublished connect cannot replace actual client state with a provisional connected state".into());
    }
    if let Some(text) = source(sources, "kernel/src/socket/unix/namespace.rs", errors) {
        if text.contains("namespace.retain(") || text.contains("namespace.remove(") {
            errors.push("ipc-owner: UNIX registry cannot scan unrelated bindings or destroy nodes under its guard".into());
        }
        let preparation = text.find("let mut prepared =");
        let address_guard = text.find("let mut bound = socket.address.lock()");
        if preparation
            .zip(address_guard)
            .is_none_or(|(prepared, guard)| prepared >= guard)
        {
            errors.push(
                "ipc-owner: UNIX binding preparation must precede address/namespace guards".into(),
            );
        }
    }
}
#[cfg(test)]
mod tests {
    fn reject(path: &str, old: &str, new: &str, expected: &str) {
        let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == path)
            .unwrap();
        assert!(source.text.contains(old));
        source.text = source.text.replace(old, new);
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "{errors:?}"
        );
    }
    #[test]
    fn actual_production_owners_satisfy_contract() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_orphan_retired_before_pending_reset_is_consumed() {
        reject(
            "kernel/src/socket/inet/tcp/io.rs",
            "tcp.state() != State::Closed || tcp.remote_endpoint().is_some()",
            "tcp.state() != State::Closed",
            "TCP orphan retirement must retain actual FIN/TIME_WAIT lifetime",
        );
    }
    #[test]
    fn rejects_handshake_shutdown_permission_lost_at_final_ack() {
        reject(
            "kernel/src/socket/inet/tcp/io.rs",
            "connection.shutdown_write = *shutdown_write",
            "connection.shutdown_write = false",
            "SYN_RECV shutdown permissions must move only at the actual final ACK",
        );
    }
    #[test]
    fn rejects_cancelled_tcp_facade_releasing_its_last_actual_slot() {
        reject(
            "kernel/src/socket/inet/tcp/lifecycle.rs",
            "*tcp = placeholder_socket()",
            "tcp.abort()",
            "TCP disconnect must preserve one actual facade slot",
        );
    }
    #[test]
    fn rejects_message_ids_from_separate_credential_publications() {
        reject(
            "kernel/src/task/model/credentials.rs",
            "self.namespace_authority(), self.resuids(), self.resgids()",
            "self.namespace_authority(), [0; 3], self.resgids()",
            "message credentials must freeze IDs",
        );
    }
    #[test]
    fn rejects_pid_override_capability_checked_in_sender_user_namespace() {
        reject(
            "kernel/src/task/model/message_identity.rs",
            "self.authority.capable(pid_owner, Capability::SYS_ADMIN)",
            "self.authority.capable(namespace, Capability::SYS_ADMIN)",
            "SCM credential claims must use native",
        );
    }
    #[test]
    fn rejects_message_controls_ignoring_empty_iterators() {
        reject(
            "kernel/src/socket/unix/stream.rs",
            "mode.is_message()",
            "mode.receives_rights()",
            "actual stream frames must preserve",
        );
    }
    #[test]
    fn rejects_stream_coalescing_reused_numeric_pid() {
        reject(
            "kernel/src/socket/credentials.rs",
            "Arc::ptr_eq(&self.pid, &other.pid)",
            "true",
            "message credentials must pin actual",
        );
    }
    #[test]
    fn rejects_rights_using_space_already_taken_by_credentials() {
        reject(
            "kernel/src/syscall/socket/control.rs",
            "control_length.saturating_sub(written)",
            "control_length",
            "native credentials precede rights",
        );
    }
    #[test]
    fn rejects_closed_socket_capacity_wait_staying_unready() {
        reject(
            "kernel/src/socket/unix/capacity.rs",
            "_ => true",
            "_ => false",
            "socket capacity wait must recheck actual",
        );
    }
    #[test]
    fn rejects_native_unix_backlog_plus_one_removed() {
        reject(
            "kernel/src/socket/unix/connection_backlog.rs",
            "limit.saturating_add(1)",
            "limit.max(1)",
            "UNIX backlog admission must include native plus-one",
        );
    }
    #[test]
    fn rejects_socket_deadline_dropped_at_registry_publication() {
        reject(
            "kernel/src/syscall/poll.rs",
            "wait_for_poll(keys, deadline,",
            "wait_for_poll(keys, None,",
            "socket wait must publish the caller-frozen deadline",
        );
    }
    #[test]
    fn rejects_socket_timeout_usec_validation_using_wrong_errno() {
        reject(
            "kernel/src/syscall/socket/timeout.rs",
            "-errno::EDOM",
            "-errno::EINVAL",
            "socket timeout codec must preserve native",
        );
    }
    #[test]
    fn rejects_autobind_without_closed_state_revalidation() {
        reject(
            "kernel/src/socket/unix/namespace.rs",
            "matches!(*state, SocketState::Closed)",
            "false",
            "UNIX bind must publish endpoint/binding once",
        );
    }
    #[test]
    fn rejects_autobind_without_socket_type_namespace() {
        reject(
            "kernel/src/socket/unix/namespace.rs",
            "NamespaceKey::Abstract(net_id, address, socket_type)",
            "NamespaceKey::Abstract(net_id, address, SocketType::Stream)",
            "UNIX bind must publish endpoint/binding once",
        );
    }
    #[test]
    fn rejects_sysvipc_pid_view_from_proc_mount_instead_of_opener() {
        reject(
            "kernel/src/fs/procfs/opened_operations.rs",
            "pid: inode.source.current_pid_namespace()",
            "pid: inode.pid_namespace.clone()",
            "sysvipc open must pin actual IPC/PID namespace",
        );
    }
    #[test]
    fn rejects_effective_gid_substitution_for_fsgid() {
        reject(
            "kernel/src/ipc/permissions.rs",
            "access.filesystem_gid == gid",
            "access.authority.effective_gid == gid",
            "DAC must use EUID, fsgid",
        );
    }
    #[test]
    fn rejects_shm_id_removal_instead_of_key_only_rmid() {
        reject(
            "kernel/src/ipc/namespace/shared_memory.rs",
            "registry.ids.withdraw_key(id)?",
            "registry.ids.remove(id)?",
            "key-only RMID",
        );
    }
    #[test]
    fn rejects_shm_arc_observers_as_nattch() {
        reject(
            "kernel/src/ipc/shared_memory.rs",
            "state.attaches == 0",
            "Arc::strong_count(&self.user) == 0",
            "actual VMA/reservation members",
        );
    }
    #[test]
    fn rejects_shm_empty_fragment_open() {
        reject(
            "kernel/src/memory/mm/shared_area.rs",
            "(original.start < start).then",
            "true.then",
            "nonempty fragments",
        );
    }
    #[test]
    fn rejects_frozen_shm_creator_as_later_vma_actor() {
        reject(
            "kernel/src/task/model/shared_memory.rs",
            "crate::task::current_tgid_identity()",
            "self.segment.creator.clone()",
            "current executor",
        );
    }
    #[test]
    fn rejects_frozen_memlock_limit_after_policy_wait() {
        reject(
            "kernel/src/ipc/namespace/shared_memory.rs",
            "let limit = read_limit()",
            "let limit = initial_limit",
            "SHM_LOCK must use actual current limit",
        );
    }
    #[test]
    fn rejects_shared_page_swap_bypassing_backing_policy() {
        reject(
            "kernel/src/memory/swappable_page.rs",
            "Some(owner) => match owner.try_shared_swap()",
            "Some(owner) => match None",
            "every shared-page swap entry",
        );
    }
    #[test]
    fn rejects_non_atomic_semop_rollback() {
        reject(
            "kernel/src/ipc/semaphore.rs",
            "operation.changes[..position].iter().rev()",
            "operation.changes[..position].iter()",
            "atomic rollback",
        );
    }
    #[test]
    fn rejects_counting_undo_observers_as_participants() {
        reject(
            "kernel/src/ipc/sem_undo.rs",
            "(state.members == 0).then",
            "(Arc::strong_count(&self.state) == 0).then",
            "actual Thread members",
        );
    }
    #[test]
    fn rejects_abandoning_the_actual_timed_event_registry() {
        reject(
            "kernel/src/task/process_table/task_wait.rs",
            "super::WAIT_REGISTRY.wake_task_event(key, result)",
            "None",
            "one actual registry membership",
        );
    }
    #[test]
    fn rejects_id_generation_exhaustion_instead_of_standard_wrap() {
        reject(
            "kernel/src/ipc/ids.rs",
            "sequence = (sequence + 1) & u32::from(u16::MAX)",
            "sequence = sequence.checked_add(1).ok_or(IpcError::NoSpace)?",
            "standard IPC64 sequence",
        );
    }
    #[test]
    fn rejects_gc_reentrant_self_edge_lock() {
        reject(
            "kernel/src/socket/unix/rights_graph.rs",
            "Arc::ptr_eq(node, successor)",
            "false",
            "root probe pins outside GRAPH",
        );
    }
    #[test]
    fn rejects_destroying_ofd_probe_under_graph_guard() {
        reject(
            "kernel/src/file/unix_passed.rs",
            "(rooted, self)",
            "(rooted, unreachable!())",
            "one actual OFD root probe",
        );
    }
    #[test]
    fn rejects_collector_allocating_a_waiter_under_pressure() {
        reject(
            "kernel/src/socket/unix/rights_graph.rs",
            "COLLECTOR.lock_prepared(&mut gate)",
            "COLLECTOR.lock().unwrap()",
            "preserve actual worker",
        );
    }
    #[test]
    fn rejects_backlog_edges_counted_as_file_references() {
        reject(
            "kernel/src/socket/unix/rights_graph.rs",
            "file_reference: uid.is_some()",
            "file_reference: true",
            "without UID/OFD reference accounting",
        );
    }
    #[test]
    fn rejects_keeping_pending_children_open_for_observer_arcs() {
        reject(
            "kernel/src/socket/unix/retirement.rs",
            "server.close_description()",
            "drop(server)",
            "retire pending children",
        );
    }
    #[test]
    fn rejects_retired_roots_treated_as_creator() {
        reject(
            "kernel/src/socket/unix/rights_owner.rs",
            "RightsOwner::Retired",
            "RightsOwner::Creator",
            "actual lifecycle capabilities",
        );
    }
    #[test]
    fn rejects_second_user_control_fetch_after_snapshot() {
        reject(
            "kernel/src/syscall/socket/control.rs",
            "ControlList::new(snapshot.bytes())",
            "ControlList::new(&header)",
            "frozen control data",
        );
    }
    #[test]
    fn rejects_peek_without_actual_ofd_root_clones() {
        reject(
            "kernel/src/socket/unix/rights.rs",
            "prepared.extend(self.files.iter().cloned())",
            "let _ = prepared;",
            "peek copies must not attach or refund",
        );
    }
    #[test]
    fn rejects_peek_advancing_actual_chunk_cursor() {
        reject(
            "kernel/src/socket/unix/stream.rs",
            "if peek {",
            "if false {",
            "one actual chunk owner",
        );
    }
    #[test]
    fn rejects_stream_head_relookup_after_minimum_selection() {
        reject(
            "kernel/src/socket/unix/stream.rs",
            "front_mut()",
            "front()",
            "allocation-free adjacent chunk reads",
        );
    }
    #[test]
    fn rejects_stream_fifo_tail_retaining_a_retired_node() {
        reject(
            "kernel/src/socket/unix/stream/queue.rs",
            "self.tail = None;",
            "let _ = self.tail;",
            "clear retired tail",
        );
    }
    #[test]
    fn rejects_unbilled_stream_rights_storage() {
        reject(
            "kernel/src/socket/unix/stream/write.rs",
            "core::mem::size_of::<Option<UnixRights>>()",
            "0",
            "stream must prepare actual barrier",
        );
    }
    #[test]
    fn rejects_frozen_scm_sender_quota_at_commit() {
        reject(
            "kernel/src/socket/unix/rights.rs",
            "self.limit.as_ref().map(|limit| limit.max_files())",
            "Some(1024)",
            "frozen syscall quota",
        );
    }
    #[test]
    fn rejects_sysvipc_continuations_discarding_the_cached_record_suffix() {
        reject(
            "kernel/src/fs/procfs/ipc_description.rs",
            "if self.consumed < self.length",
            "if false",
            "sysvipc sequential reads",
        );
    }
    #[test]
    fn rejects_physical_swap_without_shared_statistics_transaction() {
        reject(
            "kernel/src/memory/swappable_page.rs",
            "state.replace_backing(Backing::Swapped(slot))",
            "core::mem::replace(&mut state.backing, Backing::Swapped(slot))",
            "canonical physical backing transaction",
        );
    }
    #[test]
    fn rejects_shared_page_final_drop_leaking_statistics_membership() {
        reject(
            "kernel/src/memory/swappable_page.rs",
            "statistics.retire(self.backing.residency());",
            "let _ = statistics;",
            "actual membership retirement",
        );
    }
}
