//! 分区表解析：GPT（含 CRC32 校验与备份表回退）与 MBR（含扩展分区的 EBR 链）。
//!
//! 纯函数，只通过 [`SectorSource`] 读取 512 字节扇区，不依赖设备注册表，因此可在 host 上用合成镜像测试。
//! 解析顺序与 Linux `check_partition` 一致：先 EFI，再 DOS；有保护性 MBR 但 GPT 头都无效时不退回
//! MBR（否则会把保护性分区当成一块覆盖整盘的 `0xEE` 分区）。

use alloc::vec::Vec;

/// 分区表使用的扇区大小。
pub(super) const SECTOR: usize = 512;
/// 一块盘最多发布的分区数（minor 空间 `DISK_MINORS - 1`）。
pub(super) const MAX_PARTITIONS: usize = 15;
/// EBR 链的最大长度；超过即视为损坏或成环。
const MAX_LOGICAL: usize = 64;
/// GPT 分区项数组的字节上限（Linux 允许更大；这里足够 1024 项 × 128 字节）。
const MAX_GPT_ARRAY: usize = 128 * 1024;

/// 按扇区读取介质。
pub(super) trait SectorSource {
    /// 读取从 `lba` 起的 `buffer.len() / 512` 个扇区；越界或失败返回 `false`。
    fn read(&self, lba: u64, buffer: &mut [u8]) -> bool;
}

/// 一个已解析的分区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Partition {
    /// 分区号（`vda1` 的 1）；MBR 逻辑分区从 5 起，GPT 为项下标加一。
    pub(super) number: u32,
    pub(super) start: u64,
    pub(super) sectors: u64,
    /// GPT 分区项的唯一 GUID（磁盘字节序）；MBR 分区没有。
    pub(super) guid: Option<[u8; 16]>,
}

/// MBR 的 32 位磁盘签名（偏移 440）；没有有效 MBR 签名返回 `None`。
pub(super) fn mbr_signature<S: SectorSource>(source: &S) -> Option<u32> {
    let mut sector = [0u8; SECTOR];
    (source.read(0, &mut sector) && sector[510..512] == [0x55, 0xaa]).then(|| u32_at(&sector, 440))
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
}

/// IEEE 802.3 CRC32（GPT 使用；与 ext4 的 CRC32C 是不同的多项式）。
pub(super) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..u8::BITS {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// 解析 `source` 的分区表。
///
/// # Parameters
///
/// - `total_sectors`: 介质的 512 字节扇区总数；分区必须落在其中。
///
/// # Returns
///
/// 按分区号升序的分区，最多 [`MAX_PARTITIONS`] 个；没有有效分区表为空。
pub(super) fn parse<S: SectorSource>(source: &S, total_sectors: u64) -> Vec<Partition> {
    let mut first = [0u8; SECTOR];
    if !source.read(0, &mut first) || first[510..512] != [0x55, 0xaa] {
        return Vec::new();
    }
    let protective = (0..4).any(|index| first[446 + index * 16 + 4] == 0xee);
    if protective {
        // 保护性 MBR 说明这是 GPT 盘：两份头都无效时没有分区，而不是退回把整盘当成一个 0xEE 分区。
        return parse_gpt(source, total_sectors).unwrap_or_default();
    }
    parse_mbr(source, &first, total_sectors)
}

fn parse_gpt<S: SectorSource>(source: &S, total_sectors: u64) -> Option<Vec<Partition>> {
    // 主表在 LBA 1，备份头在最后一个扇区；主表损坏时由备份表恢复（Linux `is_gpt_valid` 同样如此）。
    gpt_with_header(source, 1, total_sectors)
        .or_else(|| gpt_with_header(source, total_sectors.checked_sub(1)?, total_sectors))
}

fn gpt_with_header<S: SectorSource>(
    source: &S,
    header_lba: u64,
    total_sectors: u64,
) -> Option<Vec<Partition>> {
    let mut sector = [0u8; SECTOR];
    if header_lba >= total_sectors || !source.read(header_lba, &mut sector) {
        return None;
    }
    if &sector[..8] != b"EFI PART" {
        return None;
    }
    let header_size = u32_at(&sector, 12) as usize;
    if !(92..=SECTOR).contains(&header_size) {
        return None;
    }
    let stored = u32_at(&sector, 16);
    let mut header = sector;
    header[16..20].fill(0);
    if crc32(&header[..header_size]) != stored || u64_at(&sector, 24) != header_lba {
        return None;
    }
    let first_usable = u64_at(&sector, 40);
    let last_usable = u64_at(&sector, 48);
    let entries_lba = u64_at(&sector, 72);
    let count = u32_at(&sector, 80) as usize;
    let entry_size = u32_at(&sector, 84) as usize;
    if first_usable > last_usable
        || last_usable >= total_sectors
        || !(128..=SECTOR).contains(&entry_size)
        || !entry_size.is_power_of_two()
        || count == 0
        || count.checked_mul(entry_size)? > MAX_GPT_ARRAY
    {
        return None;
    }
    let array_bytes = count * entry_size;
    let padded = array_bytes.div_ceil(SECTOR) * SECTOR;
    let mut array = Vec::new();
    array.try_reserve_exact(padded).ok()?;
    array.resize(padded, 0);
    if entries_lba.checked_add((padded / SECTOR) as u64)? > total_sectors
        || !source.read(entries_lba, &mut array)
        || crc32(&array[..array_bytes]) != u32_at(&sector, 88)
    {
        return None;
    }
    let mut partitions = Vec::new();
    for index in 0..count {
        let entry = &array[index * entry_size..(index + 1) * entry_size];
        if entry[..16].iter().all(|byte| *byte == 0) {
            continue;
        }
        let (first, last) = (u64_at(entry, 32), u64_at(entry, 40));
        if first < first_usable || last > last_usable || first > last {
            continue;
        }
        let number = index as u32 + 1;
        if number as usize > MAX_PARTITIONS {
            break;
        }
        partitions.push(Partition {
            number,
            start: first,
            sectors: last - first + 1,
            guid: entry[16..32].try_into().ok(),
        });
    }
    Some(partitions)
}

fn is_extended(kind: u8) -> bool {
    matches!(kind, 0x05 | 0x0f | 0x85)
}

fn parse_mbr<S: SectorSource>(
    source: &S,
    first: &[u8; SECTOR],
    total_sectors: u64,
) -> Vec<Partition> {
    let mut partitions = Vec::new();
    let fits = |start: u64, sectors: u64| {
        sectors != 0
            && start
                .checked_add(sectors)
                .is_some_and(|end| end <= total_sectors)
    };
    let mut extended = None;
    for slot in 0..4 {
        let entry = &first[446 + slot * 16..446 + (slot + 1) * 16];
        let (kind, start, sectors) = (
            entry[4],
            u64::from(u32_at(entry, 8)),
            u64::from(u32_at(entry, 12)),
        );
        if kind == 0 || !fits(start, sectors) {
            continue;
        }
        if is_extended(kind) {
            extended.get_or_insert((start, sectors));
        } else {
            partitions.push(Partition {
                number: slot as u32 + 1,
                start,
                sectors,
                guid: None,
            });
        }
    }
    if let Some((base, extent)) = extended {
        let mut next = 0u64; // 相对 `base` 的下一个 EBR
        for number in (5u32..).take(MAX_LOGICAL) {
            let ebr = base + next;
            let mut sector = [0u8; SECTOR];
            if ebr >= base + extent
                || !source.read(ebr, &mut sector)
                || sector[510..512] != [0x55, 0xaa]
            {
                break;
            }
            let data = &sector[446..462];
            let (kind, start, sectors) = (
                data[4],
                u64::from(u32_at(data, 8)),
                u64::from(u32_at(data, 12)),
            );
            // 逻辑分区的起点相对它自己的 EBR。
            if kind != 0 && !is_extended(kind) && fits(ebr + start, sectors) {
                partitions.push(Partition {
                    number,
                    start: ebr + start,
                    sectors,
                    guid: None,
                });
            }
            let link = &sector[462..478];
            let (link_kind, link_start) = (link[4], u64::from(u32_at(link, 8)));
            // 链接项的起点相对扩展分区；不前进（成环）或为空即结束。
            if !is_extended(link_kind) || link_start <= next {
                break;
            }
            next = link_start;
        }
    }
    partitions.retain(|partition| partition.number as usize <= MAX_PARTITIONS);
    partitions.sort_by_key(|partition| partition.number);
    partitions
}
