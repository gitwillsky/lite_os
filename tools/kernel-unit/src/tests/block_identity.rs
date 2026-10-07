//! `root=PARTUUID=` 与 UUID 用到的身份文本。

use crate::block::identity::{PartUuid, format_gpt_guid, format_uuid};
use std::vec::Vec;

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
