//! `root=PARTUUID=/UUID=/LABEL=` 用到的身份文本。

use super::block_identity::{PartUuid, ext4_identity, ext4_matches, format_gpt_guid, format_uuid};
use std::{vec, vec::Vec};

fn text(rendered: impl AsRef<[u8]>) -> Vec<u8> {
    rendered.as_ref().to_vec()
}

#[test]
fn gpt_guid_swaps_the_first_three_groups_and_uuid_does_not() {
    let bytes = [
        0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    assert_eq!(
        text(format_gpt_guid(&bytes).as_bytes()),
        b"00112233-4455-6677-8899-aabbccddeeff"
    );
    assert_eq!(
        text(format_uuid(&bytes).as_bytes()),
        b"33221100-5544-7766-8899-aabbccddeeff"
    );
}

#[test]
fn partuuid_matches_case_insensitively_for_gpt_and_mbr() {
    let guid = [
        0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    assert!(PartUuid::Gpt(guid).matches(b"00112233-4455-6677-8899-AABBCCDDEEFF"));
    assert!(!PartUuid::Gpt(guid).matches(b"00112233-4455-6677-8899-aabbccddee00"));
    let mbr = PartUuid::Mbr {
        signature: 0x4c49_5445,
        number: 2,
    };
    assert!(mbr.matches(b"4c495445-02"));
    assert!(mbr.matches(b"4C495445-02"));
    assert!(!mbr.matches(b"4c495445-2"));
    assert!(!mbr.matches(b"4c495445-03"));
}

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
