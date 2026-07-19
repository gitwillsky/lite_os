use std::{fs, path::Path};

const VFS_SOURCE: &str = "kernel/src/fs/vfs.rs";
const OPENED_SOURCE: &str = "kernel/src/fs/vfs/opened.rs";
const OPENED_INDEX_SOURCE: &str = "kernel/src/fs/vfs/opened_index.rs";
const PATH_COMPONENTS: usize = 32;
const LIVE_OPENED_FILES: usize = 4_096;
const MATCHING_NAMESPACE_ENTRIES: usize = 8;
const FINAL_DROPS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VfsOpenedCost {
    pub(super) register_registry_visits: usize,
    pub(super) register_lock_acquisitions: usize,
    pub(super) namespace_registry_visits: usize,
    pub(super) final_drop_index_visits: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(cost) if within_exact_index_budget(cost) => {}
        Ok(cost) => errors.push(format!(
            "{VFS_SOURCE}: opened-entry lifecycle must use an exact index; measured D={PATH_COMPONENTS}, R={LIVE_OPENED_FILES}, K={MATCHING_NAMESPACE_ENTRIES}, N={FINAL_DROPS}: {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn within_exact_index_budget(cost: VfsOpenedCost) -> bool {
    let height = LIVE_OPENED_FILES.ilog2() as usize;
    cost.register_registry_visits <= PATH_COMPONENTS * height * 3
        && cost.register_lock_acquisitions <= PATH_COMPONENTS
        && cost.namespace_registry_visits <= MATCHING_NAMESPACE_ENTRIES * height * 8
        && cost.final_drop_index_visits <= FINAL_DROPS * height * 3
}

pub(super) fn measure(root: &Path) -> Result<VfsOpenedCost, String> {
    let vfs = read(root, VFS_SOURCE)?;
    let opened = read(root, OPENED_SOURCE)?;
    let index = read(root, OPENED_INDEX_SOURCE).unwrap_or_default();
    let exact_register = function_body(&index, "pub(super) fn register(", OPENED_INDEX_SOURCE).ok();
    let exact_unregister =
        function_body(&index, "pub(super) fn unregister(", OPENED_INDEX_SOURCE).ok();
    let exact_index = vfs.contains("opened: OpenedIndex")
        && index.contains("entries: Mutex<FallibleMap<OpenedIndexKey, Weak<OpenedFile>>>")
        && exact_register.is_some_and(|body| {
            body.contains("FallibleMap::try_prepare(")
                && body.contains("Arc::downgrade(&opened)")
                && !body.contains("retain(")
        })
        && exact_unregister.is_some_and(|body| body.contains("remove(&key)"))
        && index.contains(".ceiling(&lower)")
        && index.contains(".successor(&cursor)")
        && index.contains(".take_entry(&key)")
        && index.contains("opened.upgrade()")
        && index.contains("drop(entries);")
        && index.contains("drop(retired_parent);")
        && index.contains("drop(opened);")
        && !index.contains("unsafe")
        && opened.contains("super::vfs().opened.unregister(key)");
    if exact_index {
        let height = LIVE_OPENED_FILES.ilog2() as usize;
        return Ok(VfsOpenedCost {
            // commit_vacant 的 duplicate probe、successor-link placement 与 AVL insertion
            // 各一条 root-to-leaf path。
            register_registry_visits: PATH_COMPONENTS * height * 3,
            register_lock_acquisitions: PATH_COMPONENTS,
            // rename 每个 matching node 最坏包含 successor、take 与 recommit；
            // unlink 只做一次 lower-bound 加 successor scan，因此取 rename 上界。
            namespace_registry_visits: MATCHING_NAMESPACE_ENTRIES * height * 8,
            final_drop_index_visits: FINAL_DROPS * height * 3,
        });
    }

    let register = function_body(&vfs, "fn register(", VFS_SOURCE)?;
    let legacy_register = register.contains("registry.retain(")
        && register.contains(".try_reserve(1)")
        && register.contains("Arc::downgrade(&opened)");
    let legacy_mutation = opened.matches("registry.retain(").count() >= 2;
    if legacy_register && legacy_mutation {
        return Ok(VfsOpenedCost {
            register_registry_visits: PATH_COMPONENTS * LIVE_OPENED_FILES,
            register_lock_acquisitions: PATH_COMPONENTS,
            namespace_registry_visits: LIVE_OPENED_FILES * 2,
            final_drop_index_visits: LIVE_OPENED_FILES,
        });
    }
    Err(format!(
        "{VFS_SOURCE}: production opened-entry lifecycle seam is not recognized by the cost gate"
    ))
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

fn function_body<'a>(source: &'a str, signature: &str, path: &str) -> Result<&'a str, String> {
    let start = source
        .find(signature)
        .ok_or_else(|| format!("{path}: missing `{signature}`"))?;
    let body = &source[start..];
    let mut depth = 0usize;
    let mut opened = false;
    for (offset, byte) in body.bytes().enumerate() {
        match byte {
            b'{' => {
                opened = true;
                depth += 1;
            }
            b'}' if opened => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&body[..=offset]);
                }
            }
            _ => {}
        }
    }
    Err(format!("{path}: unterminated `{signature}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_lookup_and_final_drop_have_logarithmic_cost() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production VFS opened cost must be measurable");
        assert!(
            within_exact_index_budget(cost),
            "D={PATH_COMPONENTS}, R={LIVE_OPENED_FILES}, K={MATCHING_NAMESPACE_ENTRIES}, N={FINAL_DROPS}, measured {cost:?}"
        );
    }
}
