use std::{fs, path::Path};

const INODE_SOURCE: &str = "kernel/src/fs/ext2/inode.rs";
const MAPPING_SOURCE: &str = "kernel/src/fs/ext2/inode/block_mapping.rs";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Ext2MappingCost {
    pub(super) classification_tracks: usize,
    pub(super) metadata_loader_tracks: usize,
    pub(super) heap_path_allocations: usize,
    pub(super) strict_sparse_traversals: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(cost) if within_budget(cost) => {}
        Ok(cost) => errors.push(format!(
            "{INODE_SOURCE}: ext2 logical-block mapping must use one allocation-free path classifier, one pointer-block loader and one strict/sparse traversal; measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn within_budget(cost: Ext2MappingCost) -> bool {
    cost == (Ext2MappingCost {
        classification_tracks: 1,
        metadata_loader_tracks: 1,
        heap_path_allocations: 0,
        strict_sparse_traversals: 1,
    })
}

pub(super) fn measure(root: &Path) -> Result<Ext2MappingCost, String> {
    let inode = read(root, INODE_SOURCE)?;
    let mapping = read(root, MAPPING_SOURCE).unwrap_or_default();
    let pointer_path = function_body(&inode, "pub(super) fn pointer_path").unwrap_or_default();
    let strict = function_body(&inode, "pub(super) fn map_block(").unwrap_or_default();
    let sparse = function_body(&inode, "pub(super) fn map_block_sparse(").unwrap_or_default();

    let classification_tracks = usize::from(pointer_path.contains("count * count"))
        + usize::from(strict.contains("ptrs_per_block"))
        + usize::from(sparse.contains("ptrs_per_block"))
        + usize::from(mapping.contains("fn resolve("));
    let metadata_loader_tracks = inode.matches("read_metadata_block(").count()
        + mapping.matches("read_metadata_block(").count();
    let heap_path_allocations =
        inode.matches("try_indices(").count() + mapping.matches("try_indices(").count();
    let strict_sparse_traversals = usize::from(strict.contains("read_indirect_block_pointer"))
        + usize::from(sparse.contains("read_indirect_block_pointer"))
        + inode.matches("for index in path.indices()").count()
        + mapping.matches("for index in path.indices()").count();

    Ok(Ext2MappingCost {
        classification_tracks,
        metadata_loader_tracks,
        heap_path_allocations,
        strict_sparse_traversals,
    })
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

fn function_body<'a>(source: &'a str, signature: &str) -> Result<&'a str, String> {
    let start = source
        .find(signature)
        .ok_or_else(|| format!("missing {signature}"))?;
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
    Err(format!("unterminated {signature}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_has_one_mapping_ownership_track() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("ext2 mapping cost must be measurable");
        assert!(within_budget(cost), "measured {cost:?}");
    }
}
