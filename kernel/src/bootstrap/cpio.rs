//! cpio `newc`（SVR4，无 CRC）归档的只读遍历。
//!
//! 每个成员：110 字节 ASCII 头（魔数 `070701` 与 13 个 8 位十六进制字段）、NUL 结尾的路径名（其后可有
//! 额外 NUL 填充）、填充到 4 字节对齐、文件内容、再填充到 4 字节对齐；以名为 `TRAILER!!!` 的成员结束。

const HEADER: usize = 110;
const MAGIC: &[u8] = b"070701";
const TRAILER: &[u8] = b"TRAILER!!!";

/// 归档中的一个成员。
pub(super) struct Entry<'a> {
    pub(super) name: &'a [u8],
    pub(super) data: &'a [u8],
}

/// 解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CpioError {
    /// 头部、名称或内容越过归档末尾。
    Truncated,
    /// 魔数或十六进制字段非法。
    Malformed,
}

fn hex(field: &[u8]) -> Result<usize, CpioError> {
    let mut value = 0usize;
    for &digit in field {
        let nibble = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            b'A'..=b'F' => digit - b'A' + 10,
            _ => return Err(CpioError::Malformed),
        };
        value = (value << 4) | usize::from(nibble);
    }
    Ok(value)
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

/// 去掉成员名开头的 `./` 与 `/`，使 `./bin/x`、`/bin/x`、`bin/x` 指向同一路径。
fn normalize(mut name: &[u8]) -> &[u8] {
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

/// 查找路径为 `path` 的成员（路径按 [`normalize`] 比较）。
///
/// # Errors
/// 归档损坏返回 [`CpioError`]；找不到返回 `Ok(None)`。
pub(super) fn find<'a>(archive: &'a [u8], path: &[u8]) -> Result<Option<Entry<'a>>, CpioError> {
    let path = normalize(path);
    let mut offset = 0;
    loop {
        let header = archive
            .get(offset..offset + HEADER)
            .ok_or(CpioError::Truncated)?;
        if &header[..6] != MAGIC {
            return Err(CpioError::Malformed);
        }
        let file_size = hex(&header[54..62])?;
        let name_size = hex(&header[94..102])?;
        let name_start = offset + HEADER;
        let name = archive
            .get(name_start..name_start + name_size)
            .ok_or(CpioError::Truncated)?;
        // 名称在第一个 NUL 处结束；其后可有任意 NUL 填充（打包器借此让文件内容页对齐）。
        let name = name.split(|byte| *byte == 0).next().unwrap_or(name);
        let data_start = align4(name_start + name_size);
        let data = archive
            .get(data_start..data_start + file_size)
            .ok_or(CpioError::Truncated)?;
        if name == TRAILER {
            return Ok(None);
        }
        if normalize(name) == path {
            return Ok(Some(Entry { name, data }));
        }
        offset = align4(data_start + file_size);
    }
}
