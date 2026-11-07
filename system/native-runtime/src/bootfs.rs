//! bootfs：cpio `newc`（SVR4，无 CRC）归档的只读访问。
//!
//! 每个成员：110 字节 ASCII 头（魔数 `070701` 与 13 个 8 位十六进制字段）、NUL 结尾的路径名（其后有
//! NUL 填充使文件内容页对齐）、文件内容、再填充到 4 字节对齐；以名为 `TRAILER!!!` 的成员结束。
//! 内容页对齐（Zircon bootfs 同样如此）使 ELF 段与大文件直接成为 bootfs VMO 的写时复制子对象，不复制字节。
//! 路径比较前去掉开头的 `./` 与 `/`，与内核装载根服务时的规则一致。

use native_abi::Status;

use crate::Vmo;

const HEADER: usize = 110;
const MAGIC: &[u8] = b"070701";
const TRAILER: &[u8] = b"TRAILER!!!";

/// 归档中的一个成员。
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    /// 规范化后的路径。
    pub name: &'a [u8],
    /// `st_mode`。
    pub mode: u32,
    /// 内容。
    pub data: &'a [u8],
    /// 内容在归档（即 bootfs VMO）内的字节偏移；打包器保证页对齐，所以文件可按页作为 VMO 子对象零复制共享。
    pub offset: usize,
}

fn hex(field: &[u8]) -> Option<usize> {
    let mut value = 0usize;
    for &digit in field {
        let nibble = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            b'A'..=b'F' => digit - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | usize::from(nibble);
    }
    Some(value)
}

/// 去掉路径开头的 `./` 与 `/`。
pub fn normalize(mut name: &[u8]) -> &[u8] {
    loop {
        if let Some(rest) = name.strip_prefix(b"./") {
            name = rest;
        } else if let Some(rest) = name.strip_prefix(b"/") {
            name = rest;
        } else {
            return name;
        }
    }
}

/// 映射在内存中的 bootfs。
#[derive(Clone, Copy)]
pub struct Bootfs {
    archive: &'static [u8],
    vmo: &'static Vmo,
}

impl Bootfs {
    /// 把 bootfs VMO 只读映射到运行时 VMAR（映射随进程存活）；VMO 也保留下来，成员按页作为它的子对象零复制地交付。
    ///
    /// # Errors
    ///
    /// 映射失败。
    pub fn map(vmo: &'static Vmo) -> Result<Self, Status> {
        let size = vmo.size()? as usize;
        let address = crate::heap::vmar().map(native_abi::vm::READ, 0, vmo, 0, size)?;
        // SAFETY: 映射只读、永不解除，内容在进程存活期间不变。
        let archive = unsafe { core::slice::from_raw_parts(address as *const u8, size) };
        Ok(Self { archive, vmo })
    }

    /// 遍历全部成员（遇到损坏或结尾即停止）。
    pub fn entries(&self) -> Entries {
        Entries {
            archive: self.archive,
            offset: 0,
        }
    }

    /// 成员 `path` 的零复制 VMO：bootfs 页对齐区间的写时复制子对象，文件位于偏移 0，大小向上取整到页
    /// （末页里越过文件长度的字节是下一成员的内容，所以只适合 ELF 这类自描述长度的内容；
    /// 需要精确长度的小文件应复制）。
    ///
    /// # Errors
    ///
    /// 成员不存在返回 `ERR_NOT_FOUND`；子对象创建失败返回内核错误。
    pub fn file_vmo(&self, path: &[u8]) -> Result<Vmo, Status> {
        let entry = self.find(path).ok_or(native_abi::ERR_NOT_FOUND)?;
        debug_assert!(entry.offset.is_multiple_of(native_abi::PAGE_SIZE));
        self.vmo.create_child(
            native_abi::vmo::CHILD_SNAPSHOT_AT_LEAST_ON_WRITE,
            entry.offset as u64,
            entry.data.len().next_multiple_of(native_abi::PAGE_SIZE) as u64,
        )
    }

    /// 零复制装载成员 `path` 的静态 ELF：各段成为 bootfs VMO 的写时复制子对象。
    ///
    /// # Errors
    ///
    /// 成员不存在返回 `ERR_NOT_FOUND`，其余同 [`crate::elf::Image::from_vmo`]。
    pub fn image(&self, path: &[u8]) -> Result<crate::elf::Image, Status> {
        let entry = self.find(path).ok_or(native_abi::ERR_NOT_FOUND)?;
        crate::elf::Image::from_vmo(self.vmo, entry.offset as u64)
    }

    /// 查找成员。
    pub fn find(&self, path: &[u8]) -> Option<Entry<'static>> {
        let path = normalize(path);
        self.entries().find(|entry| entry.name == path)
    }
}

/// 成员迭代器。
pub struct Entries {
    archive: &'static [u8],
    offset: usize,
}

impl Iterator for Entries {
    type Item = Entry<'static>;

    fn next(&mut self) -> Option<Entry<'static>> {
        let archive = self.archive;
        let header = archive.get(self.offset..self.offset + HEADER)?;
        if &header[..6] != MAGIC {
            return None;
        }
        let mode = hex(&header[14..22])? as u32;
        let file_size = hex(&header[54..62])?;
        let name_size = hex(&header[94..102])?;
        let name_start = self.offset + HEADER;
        let name = archive.get(name_start..name_start + name_size)?;
        // 名称在第一个 NUL 处结束；其后是让文件内容页对齐的 NUL 填充。
        let name = name.split(|byte| *byte == 0).next().unwrap_or(name);
        let data_start = (name_start + name_size).next_multiple_of(4);
        let data = archive.get(data_start..data_start + file_size)?;
        if name == TRAILER {
            return None;
        }
        self.offset = (data_start + file_size).next_multiple_of(4);
        Some(Entry {
            name: normalize(name),
            mode,
            data,
            offset: data_start,
        })
    }
}
