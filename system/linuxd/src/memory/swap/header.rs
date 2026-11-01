//! Linux SWAPSPACE2 v1 的 4KiB page header；不创建私有磁盘格式。

use super::SwapError;

const MAGIC_OFFSET: usize = 4096 - 10;
const BAD_PAGES_OFFSET: usize = 1536;
const MAX_BAD_PAGES: usize = (MAGIC_OFFSET - BAD_PAGES_OFFSET) / 4;

/// 已验证的 Linux header view；只在 activation preparation 中借用首个磁盘页。
pub(super) struct Header<'a> {
    bytes: &'a [u8; 4096],
    swapped: bool,
    pub(super) pages: usize,
    bad_count: usize,
}

impl<'a> Header<'a> {
    /// 检查 magic、version、inclusive last page 与坏页范围；page 0 永远不可换出。
    pub(super) fn parse(bytes: &'a [u8; 4096], storage_bytes: u64) -> Result<Self, SwapError> {
        if &bytes[MAGIC_OFFSET..] != b"SWAPSPACE2" {
            return Err(SwapError::Invalid);
        }
        let version = u32::from_ne_bytes(bytes[1024..1028].try_into().unwrap());
        let swapped = match version {
            1 => false,
            value if value.swap_bytes() == 1 => true,
            _ => return Err(SwapError::Invalid),
        };
        let read = |offset| {
            let value = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
            if swapped { value.swap_bytes() } else { value }
        };
        let last = read(1028);
        let bad_count = read(1032) as usize;
        let pages = (last as usize).checked_add(1).ok_or(SwapError::Invalid)?;
        if last == 0 || pages as u64 > storage_bytes / 4096 || bad_count > MAX_BAD_PAGES {
            return Err(SwapError::Invalid);
        }
        let header = Self {
            bytes,
            swapped,
            pages,
            bad_count,
        };
        for index in 0..bad_count {
            let page = header.bad_page(index);
            if page == 0 || page >= pages {
                return Err(SwapError::Invalid);
            }
        }
        Ok(header)
    }

    fn bad_page(&self, index: usize) -> usize {
        let offset = BAD_PAGES_OFFSET + index * 4;
        let value = u32::from_ne_bytes(self.bytes[offset..offset + 4].try_into().unwrap());
        if self.swapped {
            value.swap_bytes() as usize
        } else {
            value as usize
        }
    }

    /// 按磁盘声明顺序返回不可使用的页；bitmap preparation 在同一 owner 中去重。
    pub(super) fn bad_pages(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.bad_count).map(|index| self.bad_page(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(swapped: bool) -> [u8; 4096] {
        let mut bytes = [0u8; 4096];
        bytes[4086..].copy_from_slice(b"SWAPSPACE2");
        for (offset, value) in [(1024, 1u32), (1028, 127), (1032, 1), (1536, 126)] {
            bytes[offset..offset + 4]
                .copy_from_slice(&if swapped { value.swap_bytes() } else { value }.to_ne_bytes());
        }
        bytes
    }

    #[test]
    fn linux_header_accepts_both_endiannesses_and_inclusive_last_page() {
        for swapped in [false, true] {
            let bytes = header(swapped);
            let parsed = Header::parse(&bytes, 128 * 4096).unwrap();
            assert_eq!(parsed.pages, 128);
            assert_eq!(parsed.bad_pages().collect::<alloc::vec::Vec<_>>(), [126]);
            assert_eq!(
                Header::parse(&bytes, 127 * 4096).err(),
                Some(SwapError::Invalid)
            );
        }
    }

    #[test]
    fn linux_header_rejects_magic_version_empty_bad_count_and_bad_ranges() {
        let original = header(false);
        for (offset, value) in [
            (1024, 2u32),
            (1028, 0),
            (1032, (MAX_BAD_PAGES + 1) as u32),
            (1536, 0),
            (1536, 128),
        ] {
            let mut bytes = original;
            bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
            assert_eq!(
                Header::parse(&bytes, 128 * 4096).err(),
                Some(SwapError::Invalid)
            );
        }
        let mut bytes = original;
        bytes[4095] ^= 1;
        assert_eq!(
            Header::parse(&bytes, 128 * 4096).err(),
            Some(SwapError::Invalid)
        );
    }
}
