/// ext2 without the ext4 `dir_nlink` feature has a fixed 32,000-link ceiling.
const EXT2_LINK_MAX: u16 = 32_000;

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

/// @description Computes one ext2 link-count increment without saturation or wraparound.
pub(super) fn increment(count: u16) -> Result<u16, LinkCountError> {
    if count >= EXT2_LINK_MAX {
        Err(LinkCountError::TooMany)
    } else {
        Ok(count + 1)
    }
}

/// @description Computes one ext2 link-count decrement and rejects an impossible underflow.
pub(super) fn decrement(count: u16) -> Result<u16, LinkCountError> {
    count.checked_sub(1).ok_or(LinkCountError::Corrupt)
}

/// @description Plans the net parent-link transition before a rename edits the namespace.
/// @return 非目录或同父无替换为 None；同父替换与跨父移动返回精确最终计数。
pub(super) fn plan_rename_parent_links(
    old_parent: u16,
    new_parent: u16,
    moves_directory: bool,
    crosses_parent: bool,
    replaces_directory: bool,
) -> Result<Option<ParentLinkPlan>, LinkCountError> {
    if !moves_directory {
        return Ok(None);
    }
    if !crosses_parent {
        return if replaces_directory {
            Ok(Some(ParentLinkPlan::SameParent {
                parent: decrement(old_parent)?,
            }))
        } else {
            Ok(None)
        };
    }
    let old_parent = decrement(old_parent)?;
    let new_parent = if replaces_directory {
        new_parent
    } else {
        increment(new_parent)?
    };
    Ok(Some(ParentLinkPlan::CrossParent {
        old_parent,
        new_parent,
    }))
}
