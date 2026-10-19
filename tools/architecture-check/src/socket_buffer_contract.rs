//! 全局 core buffer 参数不能在 facade、namespace 或 packet ledger 中复制。
use super::SourceFile;
fn normalized(source: &str) -> String {
    source.split_whitespace().collect()
}
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|file| file.relative == path)
        .expect("socket buffer source")
        .text
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let owner = normalized(source(sources, "kernel/src/socket/core_buffers.rs"));
    let ledger = normalized(source(sources, "kernel/src/socket/unix/send_memory.rs"));
    let proc = normalized(source(sources, "kernel/src/fs/procfs/opened_operations.rs"));
    let abi = normalized(source(sources, "kernel/src/syscall/socket/options.rs"));
    if !owner.contains("staticLIMITS:[AtomicU32;4]")
        || owner.matches("AtomicU32::new(212_992)").count() != 2
        || owner.matches("AtomicU32::new(4<<20)").count() != 2
        || !owner.contains("value<self.minimum()||value>i32::MAXasu32")
        || !owner.contains("super::unix::minimum_buffer(direction)")
        || !ledger.contains("send_limit:CoreBufferLimit::SendDefault.value()asusize")
        || !ledger.contains("receive_limit:CoreBufferLimit::ReceiveDefault.value()asusize")
        || !ledger.contains("(valueasu32asusize).min(maximum.value()asusize)")
        || !ledger.contains("ifforced{value.max(0)asusize}")
        || !ledger.contains(".min(i32::MAXasusize/2)")
        || ledger.contains("DEFAULT_BUFFER_BYTES")
        || !abi.contains("ifforced&&!current_task().unwrap().has_initial_capability(syscall_abi::capability::Capability::NET_ADMIN)")
        || !abi.contains(".set_buffer_limit(direction,value,forced)")
    {
        errors.push("socket-buffer: one actual global tunable owner must capture independent defaults and unsigned option clamp with actual allocation minimum".into());
    }
    let write = proc.split("fnwrite_at(").nth(1).unwrap_or("");
    let policy = write
        .split("ifletSelf::CoreBufferLimit{target,resource}=self{")
        .nth(1)
        .unwrap_or("")
        .split("ifletSelf::OptmemLimit")
        .next()
        .unwrap_or("");
    if !proc.contains("Self::CoreBufferLimit{..}|Self::OptmemLimit(_)|Self::PidLimit")
        || !proc.contains("format_args!(\"{}\\n\",resource.value())")
        || !policy.starts_with("if!may_set_core_buffers(target,writer){")
        || !policy.contains("resource.minimum(),|value|{resource.set(value)")
    {
        errors.push("socket-buffer: actual proc per-open operations must read the global owner, stop subsequent reads and authorize current writer before copy/publish".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_global_owner_and_native_projection_satisfy_contract() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn copied_maximum_and_missing_writer_recheck_are_rejected() {
        for (path, old, new) in [
            (
                "kernel/src/socket/unix/send_memory.rs",
                ".min(maximum.value() as usize)",
                ".min(212_992)",
            ),
            (
                "kernel/src/fs/procfs/opened_operations.rs",
                "if !may_set_core_buffers(target, writer)",
                "if false",
            ),
            (
                "kernel/src/syscall/socket/options.rs",
                "if forced",
                "if false",
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
                errors.iter().any(|error| error.contains("socket-buffer:")),
                "{errors:?}"
            );
        }
    }
}
