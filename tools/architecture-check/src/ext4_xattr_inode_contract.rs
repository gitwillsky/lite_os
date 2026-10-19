//! EA values 必须沿实际 ext4 disk/extent/journal owner；禁止另建持久引用或 data=ordered 路径。
use super::SourceFile;

fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|source| source.relative == path)
        .expect("ext4 EA source")
        .text
}
fn normalized(text: &str) -> String {
    text.split_whitespace().collect()
}
fn before(text: &str, first: &str, second: &str) -> bool {
    matches!((text.find(first), text.find(second)), (Some(a), Some(b)) if a < b)
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let value = normalized(source(sources, "kernel/src/fs/ext4/xattr_inode.rs"));
    let xattr = normalized(source(sources, "kernel/src/fs/ext4/xattr.rs"));
    let inode = normalized(source(sources, "kernel/src/fs/ext4/inode.rs"));
    if !value.contains("u64::from(disk.i_ctime)<<32")
        || !value.contains("u64::from(disk.i_version_lo)")
        || !value.contains("crc32c(self.checksum_seed,&value)")
        || !value.contains("hash!=disk.i_atime")
        || !value.contains("ExtentTree::new(self,inode,&disk)")
        || !value.contains("allocate_block(tree.default_goal(),&block,BlockKind::Metadata)")
        || !value.contains("previous.checked_add(")
        || !value.contains("previous.checked_sub(")
        || !before(
            &value,
            "tree.remove_from(0)",
            "self.free_inode(inode,false)",
        )
        || [
            "static",
            "unsafe",
            "Atomic",
            "TaskMutex",
            "SpinLock",
            "inode_cache",
        ]
        .iter()
        .any(|marker| value.contains(marker))
    {
        errors.push("ext4-ea: disk ref/hash, real extent mapper and metadata journal must exclusively own EA values and final reclaim".into());
    }
    if !xattr.contains("le32(&self.xattr_block(disk.file_acl())?,REFCOUNT_OFFSET)==1")
        || !xattr.contains("(inline||release_external)")
        || !xattr.contains("add_reference_delta(&mutchanges,number,-1)")
        || !xattr.contains("add_reference_delta(&mutchanges,number,1)")
        || !before(
            &xattr,
            "filter(|(_,delta)|*delta>0)",
            "filter(|(_,delta)|*delta<0)",
        )
        || !xattr.contains("self.create_xattr_value(inode_num,disk,&attribute.value)")
        || !xattr.contains("self.change_xattr_value_references(inode_num,number,delta)")
        || !xattr.contains("attributes[position].value==value&&attributes[position].value_inode.is_none(){returnOk(None);}")
        || !xattr.contains("Noneifchange.flags&2!=0=>returnErr(FileSystemError::NotFound)")
    {
        errors.push("ext4-ea: physical body/block references, COW deltas, no-op and missing remove must use the single xattr transaction".into());
    }
    if !xattr.contains("letold_value_sectors=self.charged_xattr_value_sectors(disk)?")
        || !xattr.contains(".checked_sub(old_value_sectors)")
        || !xattr.contains("sectors.checked_add(new_value_sectors)")
        || !xattr.contains("size.div_ceil(self.block_size)")
    {
        errors.push("ext4-ea: parent i_blocks must charge block-rounded values and refund replacement/final release in the same transaction".into());
    }
    if !inode.contains("disk.i_flags&super::xattr_inode::EA_INODE_FLAG!=0")
        || !inode.contains("self.fs.release_xattrs(self.inode_num,&mutdisk)")
    {
        errors.push("ext4-ea: ordinary VFS must exclude hidden value inodes and main inode reclaim must release actual xattrs".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<SourceFile> {
        super::super::load_sources(&super::super::repository_root()).unwrap()
    }
    fn reject(path: &str, old: &str, new: &str, expected: &str) {
        let mut sources = fixture();
        let file = sources
            .iter_mut()
            .find(|file| file.relative == path)
            .unwrap();
        assert!(file.text.contains(old));
        file.text = file.text.replace(old, new);
        file.syntax = syn::parse_file(&file.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "{errors:?}"
        );
    }
    #[test]
    fn real_ea_owner_satisfies_contract() {
        let mut errors = Vec::new();
        check(&fixture(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_ordered_value_data() {
        reject(
            "kernel/src/fs/ext4/xattr_inode.rs",
            "BlockKind::Metadata",
            "BlockKind::Data",
            "metadata journal",
        );
    }
    #[test]
    fn rejects_freeing_values_still_owned_by_shared_block() {
        reject(
            "kernel/src/fs/ext4/xattr.rs",
            "REFCOUNT_OFFSET) == 1",
            "REFCOUNT_OFFSET) >= 1",
            "physical body/block",
        );
    }
    #[test]
    fn rejects_decrement_before_new_physical_holder() {
        reject(
            "kernel/src/fs/ext4/xattr.rs",
            "filter(|(_, delta)| *delta > 0)",
            "filter(|(_, delta)| *delta < 0)",
            "COW deltas",
        );
    }
    #[test]
    fn rejects_publishing_hidden_ea_inode() {
        reject(
            "kernel/src/fs/ext4/inode.rs",
            "disk.i_flags & super::xattr_inode::EA_INODE_FLAG != 0",
            "false",
            "ordinary VFS",
        );
    }
    #[test]
    fn rejects_missing_parent_value_charge() {
        reject(
            "kernel/src/fs/ext4/xattr.rs",
            "sectors.checked_add(new_value_sectors)",
            "Some(sectors)",
            "parent i_blocks",
        );
    }
    #[test]
    fn rejects_missing_parent_value_refund() {
        reject(
            "kernel/src/fs/ext4/xattr.rs",
            ".checked_sub(old_value_sectors)",
            ".checked_sub(0)",
            "parent i_blocks",
        );
    }
}
