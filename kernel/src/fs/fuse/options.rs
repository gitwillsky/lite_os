//! FUSE 挂载选项（Linux `fuse_fs_context` 与 `fuse_parse_param`）。
use alloc::vec::Vec;

use super::protocol;
use crate::fs::{FileSystemError, mount_options};

/// 挂载选项（Linux `fuse_fs_context`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Options {
    pub(super) fd: i32,
    pub(super) root_mode: u32,
    pub(super) user_id: u32,
    pub(super) group_id: u32,
    pub(super) default_permissions: bool,
    pub(super) allow_other: bool,
    pub(super) max_read: Option<u32>,
    pub(super) subtype: Vec<u8>,
}

impl Options {
    /// `fd`、`rootmode`、`user_id`、`group_id` 必须出现；未知选项与重复的 `subtype` 返回 `EINVAL`。
    pub(super) fn parse(options: &[u8]) -> Result<Self, FileSystemError> {
        let invalid = FileSystemError::InvalidOperation;
        let decimal = |value: Option<&[u8]>| -> Result<u32, FileSystemError> {
            core::str::from_utf8(value.ok_or(invalid)?)
                .ok()
                .and_then(|text| text.parse().ok())
                .ok_or(invalid)
        };
        let (mut fd, mut root_mode, mut user_id, mut group_id) = (None, None, None, None);
        let mut parsed = Self {
            fd: 0,
            root_mode: 0,
            user_id: 0,
            group_id: 0,
            default_permissions: false,
            allow_other: false,
            max_read: None,
            subtype: Vec::new(),
        };
        for option in mount_options::parse(options) {
            match option.key {
                b"fd" => fd = Some(decimal(option.value)? as i32),
                b"rootmode" => {
                    let mode = core::str::from_utf8(option.value.ok_or(invalid)?)
                        .ok()
                        .and_then(|text| u32::from_str_radix(text, 8).ok())
                        .ok_or(invalid)?;
                    // Linux fuse_valid_type：根必须是合法的文件类型。
                    protocol::mode_kind(mode).ok_or(invalid)?;
                    root_mode = Some(mode);
                }
                b"user_id" => user_id = Some(decimal(option.value)?),
                b"group_id" => group_id = Some(decimal(option.value)?),
                b"default_permissions" if option.value.is_none() => {
                    parsed.default_permissions = true;
                }
                b"allow_other" if option.value.is_none() => parsed.allow_other = true,
                b"max_read" => parsed.max_read = Some(decimal(option.value)?),
                b"subtype" => {
                    let value = option.value.ok_or(invalid)?;
                    if !parsed.subtype.is_empty() || value.is_empty() {
                        return Err(invalid);
                    }
                    parsed
                        .subtype
                        .try_reserve_exact(value.len())
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                    parsed.subtype.extend_from_slice(value);
                }
                _ => return Err(invalid),
            }
        }
        parsed.fd = fd.ok_or(invalid)?;
        parsed.root_mode = root_mode.ok_or(invalid)?;
        parsed.user_id = user_id.ok_or(invalid)?;
        parsed.group_id = group_id.ok_or(invalid)?;
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_options_require_connection_facts_and_reject_unknown_keys() {
        let options = Options::parse(
            b"fd=3,rootmode=40000,user_id=1000,group_id=100,allow_other,subtype=smb",
        )
        .unwrap();
        assert_eq!((options.fd, options.root_mode), (3, 0o40000));
        assert_eq!((options.user_id, options.group_id), (1000, 100));
        assert!(options.allow_other && !options.default_permissions);
        assert_eq!(options.subtype, b"smb");

        for invalid in [
            &b"rootmode=40000,user_id=0,group_id=0"[..],
            b"fd=3,rootmode=777,user_id=0,group_id=0",
            b"fd=3,rootmode=40000,user_id=0,group_id=0,blksize=4096",
            b"fd=3,rootmode=40000,user_id=0,group_id=0,subtype=a,subtype=b",
        ] {
            assert_eq!(
                Options::parse(invalid),
                Err(FileSystemError::InvalidOperation)
            );
        }
    }
}
