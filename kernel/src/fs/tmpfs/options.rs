//! tmpfs 的挂载选项（Linux `shmem_parse_one` 的子集）。

use crate::fs::mount_options::{self, parse_number, parse_scaled};

/// 一次挂载生效的 tmpfs 参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Options {
    /// 数据页上限；`None` 为不限（`size=0`）。
    pub(super) blocks: Option<u64>,
    /// inode 数上限；`None` 为不限（`nr_inodes=0`）。
    pub(super) inodes: Option<u64>,
    /// 根目录权限位（含 sticky）。
    pub(super) mode: u32,
    pub(super) uid: u32,
    pub(super) gid: u32,
}

/// 选项字符串无法接受。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct InvalidOptions;

const PAGE_SIZE: u64 = 4096;

/// 新挂载的缺省参数（Linux：数据页上限为物理内存的一半，inode 数等于物理页数，根目录 `1777`）。
pub(super) fn defaults(ram_pages: u64) -> Options {
    Options {
        blocks: Some(ram_pages / 2),
        inodes: Some(ram_pages),
        mode: 0o1777,
        uid: 0,
        gid: 0,
    }
}

/// 解析 tmpfs 选项。
///
/// 支持 `size=`（字节，可带 `k/m/g` 或 `%`）、`nr_blocks=`（页）、`nr_inodes=`、`mode=`（八进制）、
/// `uid=`、`gid=`。`huge=`、`mpol=` 等需要尚不存在的子系统，与未知 key 一样被拒绝，而不是忽略。
///
/// # Parameters
///
/// - `options`: `mount(2)` 的 `data`。
/// - `ram_pages`: 物理内存总页数，决定 `size=N%` 的基数。
/// - `base`: 未在 `options` 中出现的参数的取值：新挂载为 [`defaults`]，remount 为当前值。
///
/// # Errors
///
/// 未知 key、缺少或非法的值返回 [`InvalidOptions`]。
pub(super) fn parse(
    options: &[u8],
    ram_pages: u64,
    base: Options,
) -> Result<Options, InvalidOptions> {
    let mut parsed = base;
    for option in mount_options::parse(options) {
        let value = option.value.ok_or(InvalidOptions)?;
        match option.key {
            b"size" => {
                let bytes = match value.strip_suffix(b"%") {
                    Some(percent) => {
                        u128::from(parse_number(percent).ok_or(InvalidOptions)?)
                            * u128::from(ram_pages)
                            * u128::from(PAGE_SIZE)
                            / 100
                    }
                    None => u128::from(parse_scaled(value).ok_or(InvalidOptions)?),
                };
                let pages = u64::try_from(bytes.div_ceil(u128::from(PAGE_SIZE)))
                    .map_err(|_| InvalidOptions)?;
                parsed.blocks = (pages != 0).then_some(pages);
            }
            b"nr_blocks" => {
                let pages = parse_scaled(value).ok_or(InvalidOptions)?;
                parsed.blocks = (pages != 0).then_some(pages);
            }
            b"nr_inodes" => {
                let inodes = parse_scaled(value).ok_or(InvalidOptions)?;
                parsed.inodes = (inodes != 0).then_some(inodes);
            }
            b"mode" => {
                let mode = core::str::from_utf8(value)
                    .ok()
                    .and_then(|text| u32::from_str_radix(text, 8).ok())
                    .ok_or(InvalidOptions)?;
                parsed.mode = mode & 0o7777;
            }
            b"uid" => parsed.uid = id(value)?,
            b"gid" => parsed.gid = id(value)?,
            _ => return Err(InvalidOptions),
        }
    }
    Ok(parsed)
}

fn id(value: &[u8]) -> Result<u32, InvalidOptions> {
    parse_number(value)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or(InvalidOptions)
}
