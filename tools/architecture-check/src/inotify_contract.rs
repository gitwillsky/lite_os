//! 实际 inotify fput、ucounts、record copy 与 overflow owner 围栏。
use super::SourceFile;

fn tokens(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("inotify owner source")
        .text
        .split_whitespace()
        .collect()
}

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let file = tokens(sources, "kernel/src/file/mod.rs");
    let owner = tokens(sources, "kernel/src/fs/inotify.rs");
    let instance = tokens(sources, "kernel/src/fs/inotify/instance.rs");
    let quota = tokens(sources, "kernel/src/namespace/quota.rs");
    let syscall = tokens(sources, "kernel/src/syscall/fs/inotify.rs");
    if !file.contains("OpenFileKind::Device(device)=>device.release()")
        || !owner.contains("fnrelease(&self){self.release_instance();}")
        || !instance.contains("letmembership=self.watches.lock().take()")
        || !instance.contains("forwatchinmembership.watches.values(){unregister(watch);}")
        || !owner.contains("letadmission=watch.admission.lock().take();drop(admission);")
    {
        errors.push("inotify-owner: actual OFD fput must retire instance/watch admissions before observer Arcs disappear".into());
    }
    if !syscall.contains("task.user_resource_owner()")
        || !syscall.contains("Inotify::new(namespace,uid)")
        || !instance
            .contains("UserResourceCharge::acquire(&namespace,uid,UserResource::InotifyInstances)")
        || !instance
            .contains("UserResourceCharge::acquire(&namespace,uid,UserResource::InotifyWatches)")
        || !instance.contains("admission.owner()")
        || !quota.contains("users:FallibleMap<u32,[u32;UserResource::COUNT]>")
        || !quota.contains("charged_uid=namespace.owner()")
        || owner.contains("staticINSTANCES:")
        || instance.contains("MAX_WATCHES")
    {
        errors.push("inotify-owner: one real-UID/UserNS ancestor ledger must charge the original instance creator, without a global/per-instance substitute".into());
    }
    let dentry = tokens(sources, "kernel/src/fs/vfs/dentry.rs");
    let mutation = tokens(sources, "kernel/src/fs/vfs/mutation.rs");
    if !dentry.contains("crate::fs::notify_dentry_reclaimed(self.inode.as_ref())")
        || !owner.contains("iflinks==0{")
        || !mutation.contains(
            "lettarget_path=OpenedFile::child(target_inode.clone(),parent.clone(),&name)?",
        )
        || mutation.contains("notify_removed(")
    {
        errors.push("inotify-dput: actual canonical path must survive unlink and defer DELETE_SELF to last deleted dentry with zero links".into());
    }
    let tmpfs = tokens(sources, "kernel/src/fs/tmpfs.rs");
    let ext4 = tokens(sources, "kernel/src/fs/ext4/inode/vfs.rs");
    let cache = tokens(sources, "kernel/src/fs/page_cache/registry.rs");
    if !owner.contains("inode.notification_link_state()")
        || !cache.contains("inode.notification_link_state()")
        || !tmpfs.contains("links:AtomicU32")
        || !tmpfs.contains("Some((self.inode,self.links.load(Ordering::Relaxed)))")
        || tmpfs.contains("links:state.links")
        || tmpfs.contains("dead:bool")
        || !ext4.contains("u32::from(self.disk.lock().i_links_count)")
    {
        errors.push("inotify-dput: mutable filesystem notification identity/link facts must not allocate, fail full metadata or maintain duplicate link/dead state".into());
    }
    let read = owner
        .split("fnread(&self,")
        .nth(1)
        .expect("actual inotify read")
        .split("fnpoll(")
        .next()
        .unwrap();
    let device = tokens(sources, "kernel/src/syscall/device.rs");
    let queue = tokens(sources, "kernel/src/fs/inotify/queue.rs");
    if !read.contains(".pop(output.remaining())")
        || !read.contains("record.copy_with(|bytes|output.write(bytes))")
        || !read.contains("DeviceError::ReadFault")
        || read.contains("try_reserve")
        || owner.contains("read_gate:")
        || !device.contains("completed==0||error==DeviceError::ReadFault")
        || !queue.contains("self.events.take_entry(&key)")
        || !queue.contains("self.overflow=Some(node)")
        || !queue.contains("self.events.len()<self.maximum")
        || !queue.contains("previous.same_notification(prepared.value())")
        || !owner.contains("if!allocated||watch.mask.load(Ordering::Relaxed)&IN_ONESHOT==0")
    {
        errors.push("inotify-record: dequeue must precede allocation-free user copy, fatal EFAULT must override progress, and native overflow/oneshot ordering must hold".into());
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
    fn production_uses_actual_admission_and_native_record_lifecycle() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_backend_observer_delaying_actual_fput() {
        reject(
            "kernel/src/file/mod.rs",
            "device.release()",
            "let _ = device",
            "actual OFD fput",
        );
    }
    #[test]
    fn rejects_watches_using_a_different_resource_pool() {
        reject(
            "kernel/src/fs/inotify/instance.rs",
            "UserResource::InotifyWatches",
            "UserResource::InotifyInstances",
            "one real-UID/UserNS",
        );
    }
    #[test]
    fn rejects_partial_progress_hiding_native_record_fault() {
        reject(
            "kernel/src/syscall/device.rs",
            " || error == DeviceError::ReadFault",
            "",
            "fatal EFAULT",
        );
    }
    #[test]
    fn rejects_dentry_final_close_without_inode_notifications() {
        reject(
            "kernel/src/fs/vfs/dentry.rs",
            "crate::fs::notify_dentry_reclaimed(self.inode.as_ref());",
            "",
            "inotify-dput",
        );
    }
    #[test]
    fn rejects_oom_destroying_oneshot_mark() {
        reject(
            "kernel/src/fs/inotify.rs",
            "!allocated || ",
            "",
            "native overflow/oneshot",
        );
    }
    #[test]
    fn rejects_full_metadata_dependency_during_dentry_retirement() {
        reject(
            "kernel/src/fs/inotify.rs",
            "inode.notification_link_state()",
            "inode.metadata().ok().map(|metadata| (metadata.inode, metadata.links))",
            "must not allocate",
        );
    }
    #[test]
    fn rejects_page_cache_retirement_requiring_full_metadata() {
        reject(
            "kernel/src/fs/page_cache/registry.rs",
            "inode.notification_link_state()",
            "inode.metadata().ok().map(|metadata| (metadata.inode, metadata.links))",
            "must not allocate",
        );
    }
}
