//! MBR（含扩展/逻辑分区）与 GPT（含 CRC 校验与备份表回退）的解析。

use super::partition_table::{Partition, SECTOR, SectorSource, crc32, mbr_signature, parse};
use std::{vec, vec::Vec};

struct Image(Vec<u8>);

impl Image {
    fn new(sectors: usize) -> Self {
        Self(vec![0; sectors * SECTOR])
    }

    fn sectors(&self) -> u64 {
        (self.0.len() / SECTOR) as u64
    }

    fn sector(&mut self, lba: usize) -> &mut [u8] {
        &mut self.0[lba * SECTOR..(lba + 1) * SECTOR]
    }
}

impl SectorSource for Image {
    fn read(&self, lba: u64, buffer: &mut [u8]) -> bool {
        let start = lba as usize * SECTOR;
        match self.0.get(start..start + buffer.len()) {
            Some(bytes) => {
                buffer.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

fn mbr_entry(sector: &mut [u8], slot: usize, kind: u8, start: u32, sectors: u32) {
    let entry = &mut sector[446 + slot * 16..446 + (slot + 1) * 16];
    entry[4] = kind;
    entry[8..12].copy_from_slice(&start.to_le_bytes());
    entry[12..16].copy_from_slice(&sectors.to_le_bytes());
    sector[510] = 0x55;
    sector[511] = 0xaa;
}

fn partition(number: u32, start: u64, sectors: u64) -> Partition {
    Partition {
        number,
        start,
        sectors,
        guid: None,
    }
}

/// GPT 分区：合成镜像里第 `slot` 项的唯一 GUID 是 `[slot + 1; 16]`。
fn gpt_partition(number: u32, start: u64, sectors: u64) -> Partition {
    Partition {
        guid: Some([number as u8; 16]),
        ..partition(number, start, sectors)
    }
}

#[test]
fn blank_or_unsigned_media_has_no_partitions() {
    assert!(parse(&Image::new(64), 64).is_empty());
    let mut image = Image::new(64);
    image.sector(0)[446 + 4] = 0x83; // 没有 0x55AA 签名
    assert!(parse(&image, 64).is_empty());
}

#[test]
fn mbr_primary_partitions_keep_their_slot_numbers_and_reject_out_of_range() {
    let mut image = Image::new(8192);
    mbr_entry(image.sector(0), 0, 0x83, 2048, 2048);
    mbr_entry(image.sector(0), 2, 0x83, 4096, 1024);
    mbr_entry(image.sector(0), 3, 0x83, 8000, 1000); // 越过盘尾
    let total = image.sectors();
    assert_eq!(
        parse(&image, total),
        [partition(1, 2048, 2048), partition(3, 4096, 1024)]
    );
}

#[test]
fn mbr_logical_partitions_follow_the_ebr_chain_from_five() {
    let mut image = Image::new(16384);
    mbr_entry(image.sector(0), 0, 0x83, 2048, 1024);
    mbr_entry(image.sector(0), 1, 0x05, 4096, 8192);
    // 第一个 EBR 在 4096：逻辑分区起点相对 EBR，链接项相对扩展分区。
    mbr_entry(image.sector(4096), 0, 0x83, 2048, 1024);
    mbr_entry(image.sector(4096), 1, 0x05, 4096, 4096);
    image.sector(4096)[462..478].copy_from_slice(&{
        let mut link = [0u8; 16];
        link[4] = 0x05;
        link[8..12].copy_from_slice(&4096u32.to_le_bytes());
        link
    });
    mbr_entry(image.sector(4096 + 4096), 0, 0x83, 2048, 512);
    let total = image.sectors();
    assert_eq!(
        parse(&image, total),
        [
            partition(1, 2048, 1024),
            partition(5, 4096 + 2048, 1024),
            partition(6, 8192 + 2048, 512),
        ]
    );
}

#[test]
fn a_self_referencing_ebr_chain_terminates() {
    let mut image = Image::new(16384);
    mbr_entry(image.sector(0), 0, 0x05, 2048, 8192);
    mbr_entry(image.sector(2048), 0, 0x83, 2048, 512);
    let link = &mut image.sector(2048)[462..478];
    link[4] = 0x05;
    link[8..12].copy_from_slice(&0u32.to_le_bytes()); // 指回自己
    let total = image.sectors();
    assert_eq!(parse(&image, total), [partition(5, 2048 + 2048, 512)]);
}

fn gpt_image(sectors: usize, entries: &[(usize, u64, u64)]) -> Image {
    let mut image = Image::new(sectors);
    mbr_entry(image.sector(0), 0, 0xee, 1, (sectors - 1) as u32);
    let mut array = vec![0u8; 128 * 128];
    for (index, first, last) in entries {
        let entry = &mut array[index * 128..(index + 1) * 128];
        entry[..16].copy_from_slice(&[0xaf; 16]);
        entry[16..32].copy_from_slice(&[*index as u8 + 1; 16]);
        entry[32..40].copy_from_slice(&first.to_le_bytes());
        entry[40..48].copy_from_slice(&last.to_le_bytes());
    }
    let array_crc = crc32(&array);
    let backup = sectors as u64 - 1;
    // 主表：头在 1，项在 2..34；备份：项在 backup-32..backup，头在 backup。
    for (header_lba, entries_lba, alternate) in [(1u64, 2u64, backup), (backup, backup - 32, 1u64)]
    {
        let mut header = [0u8; SECTOR];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&header_lba.to_le_bytes());
        header[32..40].copy_from_slice(&alternate.to_le_bytes());
        header[40..48].copy_from_slice(&34u64.to_le_bytes());
        header[48..56].copy_from_slice(&(backup - 33).to_le_bytes());
        header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        header[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let header_crc = crc32(&header[..92]);
        header[16..20].copy_from_slice(&header_crc.to_le_bytes());
        image.sector(header_lba as usize).copy_from_slice(&header);
        image.0[entries_lba as usize * SECTOR..entries_lba as usize * SECTOR + array.len()]
            .copy_from_slice(&array);
    }
    image
}

#[test]
fn crc32_matches_the_ieee_check_value() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
}

#[test]
fn gpt_entries_become_partitions_numbered_by_slot() {
    let image = gpt_image(4096, &[(0, 2048, 2559), (2, 3000, 3999)]);
    let total = image.sectors();
    assert_eq!(
        parse(&image, total),
        [gpt_partition(1, 2048, 512), gpt_partition(3, 3000, 1000)]
    );
}

#[test]
fn corrupt_primary_gpt_recovers_from_the_backup_and_corrupt_both_yields_none() {
    let mut image = gpt_image(4096, &[(0, 2048, 2559)]);
    image.sector(1)[40] ^= 0xff; // 主头 CRC 失效
    let total = image.sectors();
    assert_eq!(parse(&image, total), [gpt_partition(1, 2048, 512)]);
    let backup = total as usize - 1;
    image.sector(backup)[40] ^= 0xff;
    // 保护性 MBR 存在：不退回把整盘当成一个 0xEE 分区。
    assert!(parse(&image, total).is_empty());
}

#[test]
fn gpt_entry_outside_the_usable_range_is_ignored() {
    let image = gpt_image(4096, &[(0, 10, 100), (1, 2048, 2559)]);
    let total = image.sectors();
    assert_eq!(parse(&image, total), [gpt_partition(2, 2048, 512)]);
}

#[test]
fn mbr_signature_requires_the_boot_signature() {
    let mut image = Image::new(64);
    assert_eq!(mbr_signature(&image), None);
    image.sector(0)[440..444].copy_from_slice(&0x4c49_5445u32.to_le_bytes());
    assert_eq!(mbr_signature(&image), None);
    mbr_entry(image.sector(0), 0, 0x83, 8, 8);
    assert_eq!(mbr_signature(&image), Some(0x4c49_5445));
}
