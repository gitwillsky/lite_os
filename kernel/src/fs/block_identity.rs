//! 块设备的身份文本：GPT GUID、MBR `PARTUUID`、ext4 `UUID`/`LABEL`，供 `root=` 解析使用。
//!
//! 纯函数，不依赖设备注册表。

/// 一个分区的 `PARTUUID`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartUuid {
    /// GPT 分区项的唯一 GUID（磁盘上的混合字节序原样保存）。
    Gpt([u8; 16]),
    /// MBR：32 位磁盘签名与分区号。
    Mbr { signature: u32, number: u32 },
}

/// 固定容量的文本缓冲（`PARTUUID` 最长是 GUID 的 36 字符）。
pub(super) struct Text {
    bytes: [u8; 36],
    length: usize,
}

impl Text {
    fn new() -> Self {
        Self {
            bytes: [0; 36],
            length: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        self.bytes[self.length] = byte;
        self.length += 1;
    }

    fn hex(&mut self, value: u32, digits: usize) {
        for shift in (0..digits).rev() {
            self.push(b"0123456789abcdef"[(value >> (shift * 4)) as usize & 15]);
        }
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

/// 以 RFC 4122 文本形式（`8-4-4-4-12`，字节按存储顺序）格式化；ext4 `s_uuid` 使用它。
pub(super) fn format_uuid(bytes: &[u8; 16]) -> Text {
    let mut text = Text::new();
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push(b'-');
        }
        text.hex(u32::from(*byte), 2);
    }
    text
}

/// 以 GPT/UEFI 文本形式格式化：前三组是小端整数，所以按字节序翻转（Linux `efi_guid_to_str`）。
pub(super) fn format_gpt_guid(bytes: &[u8; 16]) -> Text {
    let mut text = Text::new();
    text.hex(
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        8,
    );
    text.push(b'-');
    text.hex(u32::from(u16::from_le_bytes([bytes[4], bytes[5]])), 4);
    text.push(b'-');
    text.hex(u32::from(u16::from_le_bytes([bytes[6], bytes[7]])), 4);
    text.push(b'-');
    text.hex(u32::from(bytes[8]), 2);
    text.hex(u32::from(bytes[9]), 2);
    text.push(b'-');
    for byte in &bytes[10..16] {
        text.hex(u32::from(*byte), 2);
    }
    text
}

impl PartUuid {
    /// `PARTUUID` 的文本形式（`uevent` 的 `PARTUUID=` 与 `root=PARTUUID=` 使用同一种）。
    pub(super) fn render(&self) -> Text {
        match self {
            Self::Gpt(guid) => format_gpt_guid(guid),
            Self::Mbr { signature, number } => {
                let mut rendered = Text::new();
                rendered.hex(*signature, 8);
                rendered.push(b'-');
                rendered.hex(*number, 2);
                rendered
            }
        }
    }

    /// `text`（`root=PARTUUID=` 的值）是否指这个分区；大小写不敏感。
    pub(crate) fn matches(&self, text: &[u8]) -> bool {
        self.render().as_bytes().eq_ignore_ascii_case(text)
    }
}

/// ext4 超级块（从偏移 1024 起的 1024 字节）里的 UUID 与卷标；不是 ext4 返回 `None`。
pub(super) fn ext4_identity(superblock: &[u8]) -> Option<([u8; 16], &[u8])> {
    const MAGIC_OFFSET: usize = 0x38;
    const UUID_OFFSET: usize = 0x68;
    const LABEL_OFFSET: usize = 0x78;
    if superblock.len() < LABEL_OFFSET + 16
        || u16::from_le_bytes([superblock[MAGIC_OFFSET], superblock[MAGIC_OFFSET + 1]]) != 0xef53
    {
        return None;
    }
    let uuid = superblock[UUID_OFFSET..UUID_OFFSET + 16].try_into().ok()?;
    let label = &superblock[LABEL_OFFSET..LABEL_OFFSET + 16];
    let length = label.iter().position(|byte| *byte == 0).unwrap_or(16);
    Some((uuid, &label[..length]))
}

/// 选项值指这个 ext4 吗：`UUID=` 比 UUID 文本，`LABEL=` 比卷标（卷标区分大小写，UUID 不区分）。
pub(super) fn ext4_matches(superblock: &[u8], by_label: bool, wanted: &[u8]) -> bool {
    let Some((uuid, label)) = ext4_identity(superblock) else {
        return false;
    };
    if by_label {
        label == wanted
    } else {
        format_uuid(&uuid).as_bytes().eq_ignore_ascii_case(wanted)
    }
}
