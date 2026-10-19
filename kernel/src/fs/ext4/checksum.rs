//! ext4 `metadata_csum` 与 JBD2 `CSUM_V3` 共用的 raw crc32c。
//!
//! Linux `ext4_chksum`/`jbd2_chksum` 与 e2fsprogs `ext2fs_crc32c_le` 都对调用方给出的状态做
//! raw Castagnoli 更新，不做前后取反；superblock 以 `!0` 起始，其余 metadata 以 filesystem
//! checksum seed 或 inode seed 起始。

/// Castagnoli 多项式的 reflected 形式。
const POLYNOMIAL: u32 = 0x82F6_3B78;

const fn table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

// OWNER: 编译期生成的不可变 crc32c 查找表；无运行时写入，缺失时每个 checksum 需逐 bit 计算。
static TABLE: [u32; 256] = table();

/// 以 `crc` 为当前状态继续计算 `bytes` 的 raw crc32c。
///
/// # Parameters
///
/// - `crc`: 上一段的 raw 状态，或 metadata 的起始 seed。
/// - `bytes`: 本段输入。
///
/// # Returns
///
/// 未取反的新状态，可直接作为下一段输入或写入磁盘 checksum 字段。
pub(crate) fn crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc = TABLE[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    #[test]
    fn matches_the_castagnoli_check_value() {
        // RFC 3720 check value uses initial !0 and final inversion around the raw update.
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
    }

    #[test]
    fn split_updates_equal_one_update() {
        let whole = crc32c(0x1234_5678, b"liteos ext4 metadata");
        let split = crc32c(crc32c(0x1234_5678, b"liteos ext4"), b" metadata");
        assert_eq!(whole, split);
    }
}
