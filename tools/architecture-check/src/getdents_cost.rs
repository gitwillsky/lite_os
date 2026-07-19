use std::{fs, path::Path};

use super::SourceFile;

const SYSCALL_SOURCE: &str = "kernel/src/syscall/fs.rs";
const EXT2_VFS_SOURCE: &str = "kernel/src/fs/ext2/inode/vfs.rs";
const EXT2_DIRECTORY_SOURCE: &str = "kernel/src/fs/ext2/directory.rs";
const EXT2_CURSOR_SOURCE: &str = "kernel/src/fs/ext2/directory_cursor.rs";

const DIRECTORY_ENTRIES: usize = 128;
const ENTRIES_PER_BATCH: usize = 4;
const DIRECTORY_BLOCKS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GetdentsCost {
    pub(super) inode_full_lists: usize,
    pub(super) ext2_block_reads: usize,
    pub(super) materialized_entries: usize,
    pub(super) output_reservations: usize,
}

pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    match measure(root) {
        Ok(cost) if within_linear_budget(cost) => {}
        Ok(cost) => errors.push(format!(
            "{SYSCALL_SOURCE}: getdents64 must advance one directory cursor and reserve output once per batch; measured N={DIRECTORY_ENTRIES}, entries/batch={ENTRIES_PER_BATCH}: {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("kernel/src/fs/"))
    {
        if source.text.contains("fn list(&self)") || source.text.contains("inode.list()") {
            errors.push(format!(
                "{}: legacy full directory list track is forbidden",
                source.relative
            ));
        }
    }
    match read(root, SYSCALL_SOURCE)
        .and_then(|source| function_body(&source, "pub(crate) fn sys_getdents64").map(str::to_owned))
    {
        Ok(function) if publication_order_is_safe(&function) => {}
        Ok(_) => errors.push(format!(
            "{SYSCALL_SOURCE}: getdents64 must reserve before iteration and publish cursor only after copyout"
        )),
        Err(error) => errors.push(error),
    }
}

fn publication_order_is_safe(function: &str) -> bool {
    let positions = [
        "Dirent64Batch::try_new(",
        "inode.read_directory(",
        ".copy_to_user(",
        "*position = read.cursor",
    ]
    .map(|needle| function.find(needle));
    matches!(positions, [Some(reserve), Some(read), Some(copy), Some(publish)]
        if reserve < read && read < copy && copy < publish)
}

fn within_linear_budget(cost: GetdentsCost) -> bool {
    let batches = DIRECTORY_ENTRIES.div_ceil(ENTRIES_PER_BATCH);
    cost.inode_full_lists == 0
        && cost.ext2_block_reads <= DIRECTORY_BLOCKS + batches
        && cost.materialized_entries <= DIRECTORY_ENTRIES
        && cost.output_reservations <= batches
}

pub(super) fn measure(root: &Path) -> Result<GetdentsCost, String> {
    let syscall = read(root, SYSCALL_SOURCE)?;
    let ext2_vfs = read(root, EXT2_VFS_SOURCE)?;
    let ext2_directory = read(root, EXT2_DIRECTORY_SOURCE)?;
    let ext2_cursor = read(root, EXT2_CURSOR_SOURCE)?;
    let function = function_body(&syscall, "pub(crate) fn sys_getdents64")?;

    let batches = DIRECTORY_ENTRIES.div_ceil(ENTRIES_PER_BATCH);
    let legacy_full_list = function.contains("inode.list()")
        && ext2_vfs.contains("fn list(&self)")
        && ext2_vfs.contains("self.dir_iterate_blocks")
        && ext2_directory.contains("for block_index in 0..size / self.fs.block_size");
    if legacy_full_list {
        return Ok(GetdentsCost {
            inode_full_lists: batches,
            ext2_block_reads: batches * DIRECTORY_BLOCKS,
            materialized_entries: batches * DIRECTORY_ENTRIES,
            output_reservations: if function.contains("try_reserve_exact(record_length)") {
                DIRECTORY_ENTRIES
            } else {
                0
            },
        });
    }

    let cursor_seam = function.contains("Dirent64Batch::try_new(")
        && function.contains("inode.read_directory(*position, &mut output)")
        && !function.contains("try_reserve_exact(record_length)")
        && ext2_vfs.contains("fn read_directory(")
        && ext2_vfs.contains("self.dir_iterate_from(cursor,")
        && ext2_directory.contains("DirectoryCursor::new(start, cursor)")
        && ext2_directory.contains("directory_cursor.first_block(self.fs.block_size)")
        && ext2_directory.contains("for block_index in first_block..size / self.fs.block_size");
    let cursor_seam = cursor_seam
        && ext2_cursor.contains("self.start / block_size")
        && ext2_cursor.contains("if absolute < self.start");
    if cursor_seam {
        return Ok(GetdentsCost {
            inode_full_lists: 0,
            ext2_block_reads: batches + DIRECTORY_BLOCKS,
            materialized_entries: DIRECTORY_ENTRIES,
            output_reservations: batches,
        });
    }

    Err(format!(
        "{SYSCALL_SOURCE}: getdents64 production seam is not recognized by the complexity gate"
    ))
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

fn function_body<'a>(source: &'a str, signature: &str) -> Result<&'a str, String> {
    let start = source
        .find(signature)
        .ok_or_else(|| format!("{SYSCALL_SOURCE}: missing sys_getdents64"))?;
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
    Err(format!("{SYSCALL_SOURCE}: unterminated sys_getdents64"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_small_batches_have_linear_directory_and_allocation_cost() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production getdents64 cost must be measurable");
        assert!(
            within_linear_budget(cost),
            "N={DIRECTORY_ENTRIES}, entries/batch={ENTRIES_PER_BATCH}, measured {cost:?}"
        );
    }

    #[test]
    fn production_uses_one_cursor_track_and_copyout_before_publication() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).unwrap();
        let mut errors = Vec::new();
        check(&root, &sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
