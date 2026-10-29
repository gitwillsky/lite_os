//! 实际 OFD close、PTY admission 与 ext4 last-iput 的单一生命周期证明。
use super::SourceFile;
fn compact(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("file lifecycle owner source")
        .text
        .split_whitespace()
        .collect()
}
fn ordered(source: &str, facts: &[&str]) -> bool {
    let mut rest = source;
    for fact in facts {
        let Some(index) = rest.find(fact) else {
            return false;
        };
        rest = &rest[index + fact.len()..];
    }
    true
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let ofd = compact(sources, "kernel/src/file/mod.rs");
    let mutation = compact(sources, "kernel/src/fs/vfs/mutation.rs");
    let unlink = mutation
        .split("fnunlink_at(")
        .nth(1)
        .unwrap()
        .split("fnrename_at(")
        .next()
        .unwrap();
    let rename = mutation
        .split("fnrename_at(")
        .nth(1)
        .unwrap()
        .split("implVirtualFileSystem{")
        .next()
        .unwrap();
    let opened = compact(sources, "kernel/src/fs/vfs/opened.rs");
    let dentry = compact(sources, "kernel/src/fs/vfs/dentry.rs");
    let registry = compact(sources, "kernel/src/fs/page_cache/registry.rs");
    if mutation.contains("evict_if_unlinked")
        || !ordered(
            unlink,
            &[
                "drop(victim_guard);",
                "drop(directory_guard);",
                "drop(target_inode);",
                "drop(target_path);",
            ],
        )
        || !ordered(
            rename,
            &[
                "drop((first_victim,second_victim));",
                "drop((first_directory,second_directory,rename));",
                "drop(target);",
                "drop(target_path);",
            ],
        )
        || !ofd.contains("write._content.release_inode_access();")
        || !ordered(
            &ofd,
            &[
                "opened:Arc<OpenedFile>",
                "_write_open:Option<WritableInodeLease>",
            ],
        )
        || !opened.contains("_inode:Option<super::super::InodeAccessLease>")
        || !opened.contains("drop(self._inode.take());")
        || !dentry.contains("evict_if_unlinked(&self.inode)")
        || !registry.contains("Arc::strong_count(&file.inode)==2")
        || registry.contains("Arc::strong_count(file)==1")
        || registry.contains("holders:usize")
    {
        errors.push("file-lifetime: final dput must alone retire unlinked cache after transient/access inode pins and namespace guards; actual mount writer must outlive path/inode cleanup".into());
    }
    let tty = compact(sources, "kernel/src/devices/tty/mod.rs");
    let pty = compact(sources, "kernel/src/devices/tty/pty.rs");
    let fifo = compact(sources, "kernel/src/fs/fifo.rs");
    if !ofd.contains("OpenFileKind::Device(device)=>device.release()")
        || !tty.contains("implDeviceFileforTerminalFile{fnrelease(&self){ifletSome(slave)=&self.pty{slave.close();}}")
        || !tty.contains("implDeviceFileforPtyMasterFile{fnrelease(&self){self.0.close();}")
        || !fifo.contains("fnrelease(&self){ifletSome(reader)=&self.reader{reader.close();}ifletSome(writer)=&self.writer{writer.close();}}")
        || !pty.contains("if!self.registered.swap(false,Ordering::AcqRel){return;}")
        || !ordered(&pty, &["letnotify_hangup=", "state.master_open=false", "ifnotify_hangup{", "self.output.close();", "super::hangup(&self.pair.terminal);"])
        || !pty.contains("!state.master_open&&state.slave_opens==0")
    {
        errors.push("file-lifetime: actual device fput must consume FIFO/PTY admissions once, deliver hangup and allow index reuse despite backend observers".into());
    }
    if !pty.contains("slots:FallibleMap<u32,Weak<PtyPair>>")
        || !ordered(
            &pty,
            &[
                "letpair=loop{",
                "letterminal=Terminal::new(",
                "letpair=Arc::try_new(PtyPair{",
                "letprepared=FallibleMap::try_prepare(",
                "letmutregistry=registry.lock();",
                "registry.slots.commit_vacant(prepared);",
                "drop(registry);",
                "drop(retired);",
            ],
        )
        || !ordered(
            &pty,
            &[
                "pub(crate)fnslave_indices(",
                "letcount=registry.lock().slots.len();",
                "indices.try_reserve_exact(count)",
                "letregistry=registry.lock();",
                "ifindices.capacity()<registry.slots.len()",
            ],
        )
        || pty.contains("registry.slots.try_reserve")
    {
        errors.push("file-lifetime: PTY index publication and devpts snapshot must prepare allocations outside registry spin guards and revalidate actual membership".into());
    }
    let socket = compact(sources, "kernel/src/net/retirement.rs");
    let packet = compact(sources, "kernel/src/net/packet.rs");
    let kobject = compact(sources, "kernel/src/net/kobject.rs");
    if !ofd.contains("OpenFileKind::Socket(socket)=>socket.close_description()")
        || !socket.contains("SocketBackend::Packet(socket)=>socket.close_description()")
        || !socket.contains("SocketBackend::Kobject(socket)=>socket.close_description()")
        || !packet.contains("registry.lock().endpoints.remove(&self.id)")
        || !kobject.contains("state:Mutex<Option<KobjectSocketState>>")
        || !ordered(
            &kobject,
            &[
                "letstate=self.state.lock().take();",
                "registry.ports.take_entry(",
                "registry.endpoints.take_entry(&self.id)",
                "drop((endpoint,port,state));",
                "self.notify_read.close();",
            ],
        )
        || kobject
            .matches(".ok_or(SocketError::NotConnected)?")
            .count()
            < 4
        || !kobject.contains("letSome(state)=stateelse{returnSocketPollState::closed();}")
        || kobject.contains("implDropforKobjectSocket")
        || packet.contains("implDropforPacketSocket")
    {
        errors.push("file-lifetime: actual socket fput must retire packet/netlink registrations, reject closed observers and avoid backend Drop reentering broadcast guards".into());
    }
    let inet = compact(sources, "kernel/src/net/inet.rs");
    let options = compact(sources, "kernel/src/net/inet/options.rs");
    let owner = compact(sources, "kernel/src/net/inet/protocol_owner.rs");
    let wait = compact(sources, "kernel/src/net/inet/wait.rs");
    if !inet.contains("operation:Mutex<Option<InetEndpoint>>")
        || !ordered(
            &inet,
            &[
                "fnclose_description(",
                "self.operation.lock().take()",
                "stack.cleanup_or_defer(endpoint)",
                "self.notify_read.close();",
                "self.notify_write.close();",
            ],
        )
        || !ordered(
            &owner,
            &[
                "pub(super)fnlock(",
                "self.state.lock()",
                "self.pending_cleanup.lock().pop()",
                "super::cleanup_endpoint(&mutstate.stack,endpoint)",
                "Ok(NetworkStackGuard{",
            ],
        )
        || !options.contains("let_operation=self.open_operation()?")
        || !wait.contains("returnSocketPollState::closed()")
        || !wait.contains("if_operation.is_none(){returnSome(SocketPollState::closed());}")
    {
        errors.push("file-lifetime: actual Inet fput must take one protocol registration, drain retired ports before new task transactions and keep closed observers away from reused handles".into());
    }
    let tmpfs = compact(sources, "kernel/src/fs/tmpfs.rs");
    let rename = tmpfs
        .split("fnrename(")
        .nth(1)
        .expect("actual tmpfs rename")
        .split("enumParents")
        .next()
        .unwrap();
    if !ordered(
        rename,
        &[
            "letmutchild_state=child.state()?;",
            "letdestination_links=",
            "self.links.fetch_sub(1,Ordering::Relaxed);",
            "parents.destination().entries.insert(reserved);",
            "child_state.times.change_at(now);",
            "drop(child_state);",
            "drop(replaced_state);",
            "drop(parents);",
            "drop(namespace);",
            "drop(replaced);",
        ],
    ) || !rename.contains("moves_directory&&crosses_parent&&!replaced_directory")
        || !rename.contains("ifreplaced_directory&&!crosses_parent{")
        || rename.contains("child.state()?.times.change_at(now)")
        || !tmpfs
            .contains("letstate=self.state()?;self.require_live_directory()?;letmutcursor=cursor;")
        || !tmpfs.contains("debug_assert!(cursor>=FIRST_ENTRY_COOKIE-1)")
    {
        errors.push("file-lifetime: tmpfs rename must prepare all guards before namespace commit, apply final parent link deltas and retire victim backing outside guards; deleted directory reads must reject its actual zero-link phase".into());
    }
    let inode = compact(sources, "kernel/src/fs/ext4/inode/vfs.rs");
    let directory = compact(sources, "kernel/src/fs/ext4/directory.rs");
    let orphan = compact(sources, "kernel/src/fs/ext4/orphan.rs");
    if !inode.contains("}elseifexternally_held{drop(disk);self.fs.defer_reclaim_locked(&mutmutation,&child)?;")
        || !directory.contains("}elseifexternally_held{drop(disk);self.fs.defer_reclaim_locked(&mutmutation,&existing)?;")
        || inode.contains("disk.i_mode&0xF000!=0x4000")
        || !inode.contains("self.reclaim_locked(mutation,inode_kind::from_mode(disk.i_mode)==InodeType::Directory,)")
        || !orphan.contains("ifinode_kind::from_mode(disk.i_mode)==InodeType::Directory{disk.set_size(0);}")
        || !directory.contains("ifself.disk.lock().i_links_count==0{returnErr(FileSystemError::NotFound);}")
    {
        errors.push("file-lifetime: removed/replaced open directories must share actual ext4 orphan admission and last-iput reclamation without resurrecting the namespace".into());
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
    fn production_has_one_actual_file_lifecycle() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_unlink_and_rename_dput_before_temporary_inode_retirement() {
        for token in ["drop(target_inode);", "drop(target);"] {
            reject("kernel/src/fs/vfs/mutation.rs", token, "", "final dput");
        }
        reject(
            "kernel/src/fs/vfs/mutation.rs",
            "drop(target_inode);",
            "super::super::page_cache::evict_if_unlinked(&target_inode); drop(target_inode);",
            "final dput",
        );
    }
    #[test]
    fn rejects_fput_access_pin_surviving_final_dput() {
        reject(
            "kernel/src/file/mod.rs",
            "write._content.release_inode_access();",
            "",
            "final dput",
        );
        reject(
            "kernel/src/fs/vfs/opened.rs",
            "drop(self._inode.take());",
            "",
            "final dput",
        );
    }
    #[test]
    fn rejects_temporary_cache_io_pin_blocking_registry_retirement() {
        reject(
            "kernel/src/fs/page_cache/registry.rs",
            "Arc::strong_count(&file.inode) == 2",
            "Arc::strong_count(file) == 1 && Arc::strong_count(&file.inode) == 2",
            "final dput",
        );
    }
    #[test]
    fn rejects_backend_observer_delaying_close() {
        reject(
            "kernel/src/devices/tty/mod.rs",
            "self.0.close();",
            "",
            "actual device fput",
        );
    }
    #[test]
    fn rejects_index_reuse_waiting_for_last_observer() {
        reject(
            "kernel/src/devices/tty/pty.rs",
            "!state.master_open && state.slave_opens == 0",
            "false",
            "index reuse",
        );
    }
    #[test]
    fn rejects_open_directory_bypassing_orphan_admission() {
        reject(
            "kernel/src/fs/ext4/inode/vfs.rs",
            "} else if externally_held {",
            "} else if metadata.kind != InodeType::Directory && externally_held {",
            "open directories",
        );
    }
    #[test]
    fn rejects_rename_directory_bypassing_orphan_admission() {
        reject(
            "kernel/src/fs/ext4/directory.rs",
            "} else if externally_held {",
            "} else if existing_meta.kind != InodeType::Directory && externally_held {",
            "open directories",
        );
    }
    #[test]
    fn rejects_closed_netlink_observer_rebinding() {
        reject(
            "kernel/src/net/kobject.rs",
            "let state = state_guard.as_mut().ok_or(SocketError::NotConnected)?;",
            "let state = state_guard.as_mut().get_or_insert_with(|| unreachable!());",
            "reject closed observers",
        );
    }
    #[test]
    fn rejects_inet_close_waiting_for_backend_arc_drop() {
        reject(
            "kernel/src/net/inet.rs",
            "let retired = self.operation.lock().take();",
            "let retired = None;",
            "actual Inet fput",
        );
    }
    #[test]
    fn rejects_deferred_inet_ports_surviving_next_task_transaction() {
        reject(
            "kernel/src/net/inet/protocol_owner.rs",
            "let endpoint = self.pending_cleanup.lock().pop();",
            "let endpoint = None;",
            "retired ports before new task transactions",
        );
    }
    #[test]
    fn rejects_fallible_tmpfs_child_lock_after_namespace_commit() {
        reject(
            "kernel/src/fs/tmpfs.rs",
            "let mut child_state = child.state()?;",
            "let mut child_state = unreachable!();",
            "prepare all guards before namespace commit",
        );
    }
    #[test]
    fn rejects_tmpfs_directory_replacement_transient_parent_increment() {
        reject(
            "kernel/src/fs/tmpfs.rs",
            "moves_directory && crosses_parent && !replaced_directory",
            "moves_directory && crosses_parent",
            "final parent link deltas",
        );
    }
}
