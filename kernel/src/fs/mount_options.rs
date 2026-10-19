//! `mount(2)` 的 `data` 选项字符串：逗号分隔的 `key[=value]`（Linux `generic_parse_monolithic`）。
//!
//! 只做词法与数值解析；每个文件系统类型自己决定认识哪些 key，不认识的 key 必须以
//! `InvalidOperation`（`EINVAL`）拒绝，不能静默忽略——否则 `size=` 之类的限制会在拼写错误时失效。

/// 一个选项项。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct MountOption<'a> {
    pub(super) key: &'a [u8],
    pub(super) value: Option<&'a [u8]>,
}

/// 遍历选项字符串；空项（`a,,b`、首尾逗号）被跳过。
pub(super) fn parse(options: &[u8]) -> impl Iterator<Item = MountOption<'_>> {
    options
        .split(|&byte| byte == b',')
        .filter(|item| !item.is_empty())
        .map(|item| match item.iter().position(|&byte| byte == b'=') {
            Some(at) => MountOption {
                key: &item[..at],
                value: Some(&item[at + 1..]),
            },
            None => MountOption {
                key: item,
                value: None,
            },
        })
}

/// 按 `kstrtoull(base 0)` 解析无符号整数：`0x` 十六进制、前导 `0` 八进制、否则十进制。
///
/// # Returns
///
/// 空串、非法字符或溢出返回 `None`。
pub(super) fn parse_number(value: &[u8]) -> Option<u64> {
    let (radix, digits) = match value {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        [b'0', rest @ ..] if !rest.is_empty() => (8, rest),
        _ => (10, value),
    };
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u64, |total, &byte| {
        let digit = char::from(byte).to_digit(radix)?;
        total
            .checked_mul(u64::from(radix))?
            .checked_add(u64::from(digit))
    })
}

/// 按 `memparse` 解析带 `k/m/g/t/p/e`（1024 的幂，大小写不敏感）后缀的数。
///
/// # Returns
///
/// 非法或溢出返回 `None`。
pub(super) fn parse_scaled(value: &[u8]) -> Option<u64> {
    let (digits, shift) = match value.split_last() {
        Some((suffix, rest)) if suffix.is_ascii_alphabetic() => {
            let shift = match suffix.to_ascii_lowercase() {
                b'k' => 10,
                b'm' => 20,
                b'g' => 30,
                b't' => 40,
                b'p' => 50,
                b'e' => 60,
                _ => return None,
            };
            (rest, shift)
        }
        _ => (value, 0),
    };
    let number = parse_number(digits)?;
    number
        .checked_mul(1u64.checked_shl(shift)?)
        .filter(|scaled| scaled >> shift == number)
}

/// 不接受任何选项的文件系统类型：出现任何选项项都返回 `false`。
pub(crate) fn is_empty(options: &[u8]) -> bool {
    parse(options).next().is_none()
}
