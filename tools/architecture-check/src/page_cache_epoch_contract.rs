//! page-cache 不阻塞填页的版本协议：改写者经 `begin_mutation` 标记，填页在 `pages` 锁内校验后才发布。
use super::SourceFile;

fn source(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("page cache actual source")
        .text
        .split_whitespace()
        .collect()
}

fn count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let cache = source(sources, "kernel/src/fs/page_cache.rs");
    let write = source(sources, "kernel/src/fs/page_cache/regular_write.rs");
    let nowait = source(sources, "kernel/src/fs/page_cache/nowait_read.rs");
    // 每个持有 `operation` 并改变文件内容/EOF 的入口（write、append、truncate、allocate）都必须紧接着标记；
    // 漏掉一个，异步填页就可能把改写之前读到的旧块发布进 cache。
    if count(
        &write,
        ".operation.lock().map_err(|_|FileSystemError::OutOfMemory)?;let_epoch=self.file.begin_mutation();",
    ) != 2
        || count(
            &cache,
            ".operation.lock().map_err(|_|FileSystemError::OutOfMemory)?;let_epoch=file.begin_mutation();",
        ) != 2
        || !cache.contains("drop(_epoch);drop(_operation);")
    {
        errors.push("page-cache-epoch: write, append, truncate and allocate must begin_mutation immediately after taking the operation lock, and truncate must end it before releasing operation".into());
    }
    // 发布必须在 pages 锁内校验版本，且发布不分配（节点与页已预分配）；填页读存储时不得持有任何锁。
    let publish = nowait
        .split("fnstep(&mutself)->Result<FillStep")
        .nth(1)
        .unwrap_or("");
    let read_before_lock = publish
        .find("self.read.step(")
        .zip(publish.find("self.file.pages.lock()"))
        .is_some_and(|(read, lock)| read < lock);
    if !read_before_lock
        || !publish.contains("self.file.storage_epoch.is_current(self.epoch)")
        || !publish.contains("commit_vacant(slot.fill(self.index,self.page.clone()))")
        || publish.contains("Arc::try_new")
        || nowait.contains(".operation")
    {
        errors.push("page-cache-epoch: non-blocking fill must read storage before taking the pages lock, check the storage epoch inside it, publish without allocating, and never take the operation lock".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_page_cache_satisfies_epoch_contract() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn rejects_unmarked_mutation_and_unchecked_publication() {
        for (path, old, new) in [
            (
                "kernel/src/fs/page_cache/regular_write.rs",
                "let _epoch = self.file.begin_mutation();\n        let offset = self.file.inode.size();",
                "let offset = self.file.inode.size();",
            ),
            (
                "kernel/src/fs/page_cache/nowait_read.rs",
                "if self.file.storage_epoch.is_current(self.epoch) {",
                "if true {",
            ),
        ] {
            let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
            let source = sources
                .iter_mut()
                .find(|source| source.relative == path)
                .unwrap();
            assert!(source.text.contains(old), "{old}");
            source.text = source.text.replace(old, new);
            source.syntax = syn::parse_file(&source.text).unwrap();
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(
                errors
                    .iter()
                    .any(|error| error.starts_with("page-cache-epoch:")),
                "{errors:?}"
            );
        }
    }
}
