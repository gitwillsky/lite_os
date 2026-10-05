use std::{fs, path::Path};

const INODE_SOURCE: &str = "kernel/src/fs/ext4/inode.rs";
const EXTENT_SOURCE: &str = "kernel/src/fs/ext4/extent.rs";
const EXT4_ROOT: &str = "kernel/src/fs/ext4";
/// ext2 间接块映射的标识；任何一个重新出现都表示第二套 logical-block mapping。
const RETIRED_INDIRECT_MARKERS: &[&str] = &["BlockPath", "pointer_block", "decode_pointer_block"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Ext4MappingCost {
    /// extent tree 遍历实现数；只能由 `extent.rs` 唯一拥有。
    pub(super) lookup_owners: usize,
    /// 只读 lookup 中的 heap 分配点。
    pub(super) lookup_heap_allocations: usize,
    /// inode sparse mapping 委托给 extent lookup 的次数。
    pub(super) sparse_delegations: usize,
    /// ext4 源码中残留的间接块映射标识数。
    pub(super) indirect_tracks: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(cost) if within_budget(cost) => {}
        Ok(cost) => errors.push(format!(
            "{EXTENT_SOURCE}: ext4 logical-block mapping must have one allocation-free extent lookup owner and no indirect-block track; measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn within_budget(cost: Ext4MappingCost) -> bool {
    cost == (Ext4MappingCost {
        lookup_owners: 1,
        lookup_heap_allocations: 0,
        sparse_delegations: 1,
        indirect_tracks: 0,
    })
}

fn ext4_sources(root: &Path, directory: &Path, sources: &mut Vec<String>) -> Result<(), String> {
    let entries = fs::read_dir(root.join(directory))
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        let relative = path.strip_prefix(root).map_err(|error| error.to_string())?;
        if path.is_dir() {
            ext4_sources(root, relative, sources)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(read(root, &relative.to_string_lossy())?);
        }
    }
    Ok(())
}

pub(super) fn measure(root: &Path) -> Result<Ext4MappingCost, String> {
    let inode = read(root, INODE_SOURCE)?;
    let extent = read(root, EXTENT_SOURCE)?;
    let mut sources = Vec::new();
    ext4_sources(root, Path::new(EXT4_ROOT), &mut sources)?;
    let lookup = function_body(&extent, "pub(super) fn lookup(")?;
    let sparse = function_body(&inode, "pub(super) fn map_block_sparse(")?;

    let lookup_owners = sources
        .iter()
        .map(|source| source.matches("fn lookup(&self, logical: u32)").count())
        .sum();
    let lookup_heap_allocations = ["try_zeroed", "Vec::", "try_reserve", ".to_vec()"]
        .iter()
        .map(|marker| lookup.matches(marker).count())
        .sum();
    let sparse_delegations = sparse.matches("tree.lookup(").count();
    let indirect_tracks = sources
        .iter()
        .map(|source| {
            RETIRED_INDIRECT_MARKERS
                .iter()
                .map(|marker| source.matches(marker).count())
                .sum::<usize>()
        })
        .sum();

    Ok(Ext4MappingCost {
        lookup_owners,
        lookup_heap_allocations,
        sparse_delegations,
        indirect_tracks,
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
        let cost = measure(&root).expect("ext4 mapping cost must be measurable");
        assert!(within_budget(cost), "measured {cost:?}");
    }
}
