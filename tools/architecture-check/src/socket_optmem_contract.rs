//! 控制消息必须在实际 socket budget 预收费；发送/失败结束后才退款。
use super::SourceFile;
fn normalized(source: &str) -> String {
    source.split_whitespace().collect()
}
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|file| file.relative == path)
        .expect("optmem source")
        .text
}
fn before(source: &str, first: &str, second: &str) -> bool {
    matches!((source.find(first), source.find(second)), (Some(a), Some(b)) if a < b)
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let memory = normalized(source(sources, "kernel/src/socket/control_memory.rs"));
    let network = normalized(source(sources, "kernel/src/socket/net_namespace.rs"));
    let import = normalized(source(sources, "kernel/src/syscall/socket/control_list.rs"));
    let message = normalized(source(sources, "kernel/src/syscall/socket/message.rs"));
    let send = message
        .split("pub(crate)fnsys_sendmsg(")
        .nth(1)
        .unwrap_or("")
        .split("pub(crate)fnsys_sendto(")
        .next()
        .unwrap_or("");
    if !memory.contains("namespace:Arc<NetNamespace>")
        || !memory.contains("letmaximum=self.namespace.optmem_max()")
        || !memory.contains("ifbytes>maximum{returnNone;}")
        || !memory.contains("ifnext>=maximum{returnNone;}")
        || !memory.contains("self.used.compare_exchange_weak(used,next,")
        || !memory.contains("self.memory.used.fetch_sub(self.bytes,")
        || !network.contains("optmem_max:AtomicU32::new(128*1024)")
        || !network.contains("self.optmem_max.store(value,Ordering::Relaxed)")
        || !import.contains("_charge:crate::socket::ControlMemoryCharge")
        || !before(
            &import,
            "iflength<=INLINE_CONTROL",
            "socket.reserve_control_memory(length)",
        )
        || !before(
            &import,
            "socket.reserve_control_memory(length)",
            "UserInputStaging::try_new(length)",
        )
        || !before(
            &import,
            "UserInputStaging::try_new(length)",
            "task.copy_from_user_uninit(",
        )
        || !before(send, "letresult=send_message(", "drop(snapshot)")
    {
        errors.push("socket-optmem: actual per-socket atomic admission must charge requested bytes against captured NET policy before allocation/copy, retain heap token through protocol send and refund exactly once".into());
    }
    let prepare = normalized(source(sources, "kernel/src/socket/preparation.rs"));
    let abi = normalized(source(sources, "kernel/src/syscall/socket.rs"));
    let proc = normalized(source(
        sources,
        "kernel/src/storage/fs/procfs/opened_operations.rs",
    ));
    if !prepare.contains("letmemory=super::SocketMemory::try_new(namespace)?")
        || !prepare.contains("memory:self.memory")
        || !abi.contains("task.net_namespace()")
        || !abi.contains("listener.net_namespace(),")
        || !proc.contains("OptmemLimit(Arc<crate::socket::NetNamespace>)")
        || !proc.contains("if!target.may_set_limits(writer){")
        || !proc.contains("target.set_optmem_max(value,writer)")
    {
        errors.push("socket-optmem: create/accept and proc operations must capture one actual NET object, with current writer authorization and no current-Task redirection of existing fd".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_socket_allocation_and_native_control_lifetime_satisfy_contract() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn overshoot_unmetered_copy_early_refund_and_accept_redirection_are_rejected() {
        for (path, old, new) in [
            (
                "kernel/src/socket/control_memory.rs",
                "next >= maximum",
                "next > maximum",
            ),
            (
                "kernel/src/syscall/socket/control_list.rs",
                ".reserve_control_memory(length)",
                ".reserve_control_memory(0)",
            ),
            (
                "kernel/src/syscall/socket/message.rs",
                "drop(snapshot);",
                "drop(ancillary.credentials);",
            ),
            (
                "kernel/src/syscall/socket.rs",
                "listener.net_namespace(),",
                "task.net_namespace(),",
            ),
        ] {
            let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
            let actual = sources
                .iter_mut()
                .find(|file| file.relative == path)
                .unwrap();
            assert!(actual.text.contains(old));
            actual.text = actual.text.replace(old, new);
            syn::parse_file(&actual.text).unwrap();
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(
                errors.iter().any(|error| error.contains("socket-optmem:")),
                "{errors:?}"
            );
        }
    }
}
