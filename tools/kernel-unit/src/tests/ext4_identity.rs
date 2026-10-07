//! `root=UUID=/LABEL=` 用到的 ext4 超级块身份。

use super::ext4_identity::{ext4_identity, ext4_matches};
use std::{vec, vec::Vec};

fn superblock(uuid: [u8; 16], label: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 1024];
    bytes[0x38..0x3a].copy_from_slice(&0xef53u16.to_le_bytes());
    bytes[0x68..0x78].copy_from_slice(&uuid);
    bytes[0x78..0x78 + label.len()].copy_from_slice(label);
    bytes
}

#[test]
fn ext4_identity_requires_the_magic_and_trims_the_label_at_nul() {
    let uuid = [0xab; 16];
    let sb = superblock(uuid, b"LITEOS");
    let (found, label) = ext4_identity(&sb).unwrap();
    assert_eq!((found, label), (uuid, &b"LITEOS"[..]));
    assert!(ext4_identity(&vec![0u8; 1024]).is_none());
    // 16 字节卷标没有 NUL 终止时取满 16 字节。
    let full = superblock(uuid, b"0123456789abcdef");
    assert_eq!(ext4_identity(&full).unwrap().1, b"0123456789abcdef");
}

#[test]
fn label_is_case_sensitive_and_uuid_is_not() {
    let sb = superblock([0x12; 16], b"LITEOS");
    assert!(ext4_matches(&sb, true, b"LITEOS"));
    assert!(!ext4_matches(&sb, true, b"liteos"));
    assert!(ext4_matches(
        &sb,
        false,
        b"12121212-1212-1212-1212-121212121212"
    ));
    assert!(ext4_matches(
        &sb,
        false,
        b"12121212-1212-1212-1212-121212121212"
            .to_ascii_uppercase()
            .as_slice()
    ));
    assert!(!ext4_matches(&vec![0u8; 1024], true, b"LITEOS"));
}
