//! ext4 htree 使用的 half_md4 directory hash（Linux `fs/ext4/hash.c`）。
//!
//! 固定 profile 只接受 half_md4；signed/unsigned 只决定 name byte 的扩展方式，由
//! superblock `s_flags` 选择，属于同一算法的参数而非第二套 hash。

/// htree 中保留给 EOF 的 major hash；真实 hash 不得等于 `EOF << 1`。
const HTREE_EOF_32BIT: u32 = 0x7FFF_FFFF;

/// 一个 directory entry 名称的 htree 排序键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DirectoryHash {
    /// dx_entry 比较使用的 major hash，最低位恒为零（作为 collision continuation 标记）。
    pub(crate) major: u32,
    /// 同 major hash 内的次级排序键。
    pub(crate) minor: u32,
}

/// name byte 扩展为 hash 输入字时的符号语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashSignedness {
    /// `EXT2_FLAGS_SIGNED_HASH`：byte 按 `signed char` 扩展。
    Signed,
    /// `EXT2_FLAGS_UNSIGNED_HASH`：byte 按 `unsigned char` 扩展。
    Unsigned,
}

const K2: u32 = 0o13240474631;
const K3: u32 = 0o15666365641;

fn f(x: u32, y: u32, z: u32) -> u32 {
    z ^ (x & (y ^ z))
}

fn g(x: u32, y: u32, z: u32) -> u32 {
    (x & y).wrapping_add((x ^ y) & z)
}

fn h(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}

fn round(function: fn(u32, u32, u32) -> u32, a: &mut u32, b: u32, c: u32, d: u32, x: u32, s: u32) {
    *a = a
        .wrapping_add(function(b, c, d))
        .wrapping_add(x)
        .rotate_left(s);
}

fn half_md4_transform(buf: &mut [u32; 4], input: &[u32; 8]) {
    let [mut a, mut b, mut c, mut d] = *buf;
    // 1. Round 1：F，K1 = 0。
    round(f, &mut a, b, c, d, input[0], 3);
    round(f, &mut d, a, b, c, input[1], 7);
    round(f, &mut c, d, a, b, input[2], 11);
    round(f, &mut b, c, d, a, input[3], 19);
    round(f, &mut a, b, c, d, input[4], 3);
    round(f, &mut d, a, b, c, input[5], 7);
    round(f, &mut c, d, a, b, input[6], 11);
    round(f, &mut b, c, d, a, input[7], 19);
    // 2. Round 2：G + K2。
    round(g, &mut a, b, c, d, input[1].wrapping_add(K2), 3);
    round(g, &mut d, a, b, c, input[3].wrapping_add(K2), 5);
    round(g, &mut c, d, a, b, input[5].wrapping_add(K2), 9);
    round(g, &mut b, c, d, a, input[7].wrapping_add(K2), 13);
    round(g, &mut a, b, c, d, input[0].wrapping_add(K2), 3);
    round(g, &mut d, a, b, c, input[2].wrapping_add(K2), 5);
    round(g, &mut c, d, a, b, input[4].wrapping_add(K2), 9);
    round(g, &mut b, c, d, a, input[6].wrapping_add(K2), 13);
    // 3. Round 3：H + K3。
    round(h, &mut a, b, c, d, input[3].wrapping_add(K3), 3);
    round(h, &mut d, a, b, c, input[7].wrapping_add(K3), 9);
    round(h, &mut c, d, a, b, input[2].wrapping_add(K3), 11);
    round(h, &mut b, c, d, a, input[6].wrapping_add(K3), 15);
    round(h, &mut a, b, c, d, input[1].wrapping_add(K3), 3);
    round(h, &mut d, a, b, c, input[5].wrapping_add(K3), 9);
    round(h, &mut c, d, a, b, input[0].wrapping_add(K3), 11);
    round(h, &mut b, c, d, a, input[4].wrapping_add(K3), 15);
    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

/// Linux `str2hashbuf_{signed,unsigned}`：把最多 32 byte 名称片段打包为 8 个输入字。
fn string_to_words(name: &[u8], signedness: HashSignedness) -> [u32; 8] {
    let length = name.len() as u32;
    let mut pad = length | (length << 8);
    pad |= pad << 16;
    let mut words = [pad; 8];
    let mut value = pad;
    let mut word = 0;
    for (index, byte) in name.iter().take(32).enumerate() {
        let extended = match signedness {
            HashSignedness::Signed => i32::from(*byte as i8) as u32,
            HashSignedness::Unsigned => u32::from(*byte),
        };
        value = extended.wrapping_add(value << 8);
        if index % 4 == 3 {
            words[word] = value;
            word += 1;
            value = pad;
        }
    }
    if word < words.len() {
        words[word] = value;
    }
    words
}

/// 计算一个名称的 ext4 half_md4 htree hash。
///
/// # Parameters
///
/// - `name`: directory entry 的 raw 名称 bytes。
/// - `seed`: superblock `s_hash_seed`；全零时使用 MD4 初始常量。
/// - `signedness`: superblock 选择的 byte 扩展语义。
///
/// # Returns
///
/// major（最低位清零且避开 EOF 值）与 minor hash。
pub(crate) fn half_md4(name: &[u8], seed: [u32; 4], signedness: HashSignedness) -> DirectoryHash {
    let mut buf = if seed.iter().any(|word| *word != 0) {
        seed
    } else {
        [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476]
    };
    let mut remaining = name;
    loop {
        half_md4_transform(&mut buf, &string_to_words(remaining, signedness));
        if remaining.len() <= 32 {
            break;
        }
        remaining = &remaining[32..];
    }
    let mut major = buf[1] & !1;
    if major == HTREE_EOF_32BIT << 1 {
        major = (HTREE_EOF_32BIT - 1) << 1;
    }
    DirectoryHash {
        major,
        minor: buf[2],
    }
}

#[cfg(test)]
mod tests {
    use super::{DirectoryHash, HashSignedness, half_md4};

    // e2fsprogs 1.47.4 `debugfs dx_hash -s 0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9`。
    const SEED: [u32; 4] = [0x3D2C_1B0A, 0x7160_5F4E, 0xB5A4_9382, 0xF9E8_D7C6];

    fn hash(major: u32, minor: u32) -> DirectoryHash {
        DirectoryHash { major, minor }
    }

    #[test]
    fn matches_e2fsprogs_signed_vectors() {
        let cases: [(&[u8], u32, u32); 5] = [
            (b"hello", 0xECB6_CEBC, 0x02DD_7B7C),
            (b"a", 0xFE11_1624, 0x0D1E_A59A),
            (b"liteos-ext4", 0xC272_ACD2, 0x440A_59CB),
            (
                b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH",
                0x9EA3_5012,
                0x85C7_6DC0,
            ),
            ("é中".as_bytes(), 0x4B6A_044A, 0x2665_9B95),
        ];
        for (name, major, minor) in cases {
            assert_eq!(
                half_md4(name, SEED, HashSignedness::Signed),
                hash(major, minor)
            );
        }
    }

    #[test]
    fn unsigned_differs_only_for_high_bytes() {
        assert_eq!(
            half_md4(b"hello", SEED, HashSignedness::Unsigned),
            hash(0xECB6_CEBC, 0x02DD_7B7C)
        );
        assert_eq!(
            half_md4("é中".as_bytes(), SEED, HashSignedness::Unsigned),
            hash(0x2446_2B7C, 0x47CB_07D8)
        );
    }

    // `debugfs dx_hash -s 00000000-0000-0000-0000-000000000000 hello`。
    #[test]
    fn zero_seed_uses_md4_initial_constants() {
        assert_eq!(
            half_md4(b"hello", [0; 4], HashSignedness::Signed),
            hash(0x1746_DA32, 0x4200_13B5)
        );
    }
}
