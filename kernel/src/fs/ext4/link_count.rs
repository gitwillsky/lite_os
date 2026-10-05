//! ext4 link count 规则：regular link 上限 `EXT4_LINK_MAX`，directory 受 `dir_nlink` 约束。
//!
//! `dir_nlink` 下 directory link count 超过上限后固定为 1，表示“子目录数未知”；之后增减都
//! 保持 1（Linux `ext4_inc_count`/`ext4_dec_count`）。

/// Linux `EXT4_LINK_MAX`。
pub(super) const EXT4_LINK_MAX: u16 = 65_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LinkCountError {
    TooMany,
    Corrupt,
}

/// Final parent counts for one directory rename, computed before namespace edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParentLinkPlan {
    SameParent { parent: u16 },
    CrossParent { old_parent: u16, new_parent: u16 },
}

/// 非 directory 的 hard link 增加；达到上限返回 `TooMany`。
pub(super) fn increment_file(count: u16) -> Result<u16, LinkCountError> {
    if count >= EXT4_LINK_MAX {
        Err(LinkCountError::TooMany)
    } else {
        Ok(count + 1)
    }
}

/// 非 directory 的 hard link 减少；下溢表示 on-disk 计数损坏。
pub(super) fn decrement_file(count: u16) -> Result<u16, LinkCountError> {
    count.checked_sub(1).ok_or(LinkCountError::Corrupt)
}

/// 新增子目录时 parent directory 的 link count。
pub(super) fn increment_directory(count: u16) -> Result<u16, LinkCountError> {
    match count {
        0 => Err(LinkCountError::Corrupt),
        1 => Ok(1),
        count if count >= EXT4_LINK_MAX => Ok(1),
        count => Ok(count + 1),
    }
}

/// 删除子目录时 parent directory 的 link count；拥有子目录的 parent 至少为 3。
pub(super) fn decrement_directory(count: u16) -> Result<u16, LinkCountError> {
    match count {
        1 => Ok(1),
        0..=2 => Err(LinkCountError::Corrupt),
        count => Ok(count - 1),
    }
}

/// directory rename 前计算 parent link 的最终值。
///
/// # Returns
///
/// 同 parent 且不替换 directory 时为 None；其余返回精确最终计数。
pub(super) fn plan_rename_parent_links(
    old_parent: u16,
    new_parent: u16,
    crosses_parent: bool,
    replaces_directory: bool,
) -> Result<Option<ParentLinkPlan>, LinkCountError> {
    if !crosses_parent {
        return if replaces_directory {
            Ok(Some(ParentLinkPlan::SameParent {
                parent: decrement_directory(old_parent)?,
            }))
        } else {
            Ok(None)
        };
    }
    let old_parent = decrement_directory(old_parent)?;
    let new_parent = if replaces_directory {
        new_parent
    } else {
        increment_directory(new_parent)?
    };
    Ok(Some(ParentLinkPlan::CrossParent {
        old_parent,
        new_parent,
    }))
}
