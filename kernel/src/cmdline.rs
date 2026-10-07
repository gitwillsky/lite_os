//! Linux kernel command line（DTB `/chosen/bootargs`）的解析。
//!
//! 分词与引号规则同 Linux `next_arg`；内核消费的参数取出，其余按 Linux `unknown_bootoption`
//! 转交 init：`name=value` 进入 init 环境，单独的词与 `--` 之后的全部词进入 init argv，带 `.`
//! 的模块参数忽略。

use alloc::vec::Vec;

/// Linux `MAX_INIT_ARGS`/`MAX_INIT_ENVS`。
const MAX_INIT_ENTRIES: usize = 32;
/// Linux `envp_init` 的初始项；command line 的同名变量覆盖它们。
const INITIAL_ENVIRONMENT: [&[u8]; 2] = [b"HOME=/", b"TERM=linux"];

/// 内核消费的启动参数与转交 init 的剩余参数。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct KernelParameters {
    /// `init=`：init 程序路径。
    pub(crate) init: Option<Vec<u8>>,
    /// `root=`：根块设备（`/dev/<name>` 或 `MAJ:MIN`）。
    pub(crate) root: Option<Vec<u8>>,
    /// `rootfstype=`。
    pub(crate) root_filesystem_type: Option<Vec<u8>>,
    /// 最后一个 `ro`/`rw` 选择只读根。
    pub(crate) read_only_root: bool,
    /// 最后一个 `console=` 的设备名（`,` 之前的部分）。
    pub(crate) console: Option<Vec<u8>>,
    /// `loglevel=`/`quiet`/`debug` 给出的 Linux console loglevel。
    pub(crate) log_level: Option<u8>,
    /// init 的 argv（不含 argv[0]）。
    pub(crate) init_arguments: Vec<Vec<u8>>,
    /// init 的环境（`name=value`），以 `HOME=/`、`TERM=linux` 开始。
    pub(crate) init_environment: Vec<Vec<u8>>,
}

/// command line 无法被接受的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandLineError {
    /// 转交 init 的 argv 超过 32 项（Linux 同样 panic）。
    TooManyArguments,
    /// 转交 init 的环境超过 32 项。
    TooManyEnvironment,
    /// `loglevel=` 的值不是十进制整数。
    InvalidLogLevel,
    /// 分配失败。
    OutOfMemory,
}

fn owned(bytes: &[u8]) -> Result<Vec<u8>, CommandLineError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| CommandLineError::OutOfMemory)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

fn push(
    entries: &mut Vec<Vec<u8>>,
    entry: Vec<u8>,
    full: CommandLineError,
) -> Result<(), CommandLineError> {
    if entries.len() >= MAX_INIT_ENTRIES {
        return Err(full);
    }
    entries
        .try_reserve(1)
        .map_err(|_| CommandLineError::OutOfMemory)?;
    entries.push(entry);
    Ok(())
}

/// 取下一个参数（Linux `next_arg`）：返回 `(参数, 值, 剩余)`，引号内的空白不分词，包围值的
/// 引号被剥除。
fn next_argument(line: &[u8]) -> (&[u8], Option<&[u8]>, &[u8]) {
    let (mut start, mut quoted) = (0, false);
    if line.first() == Some(&b'"') {
        start = 1;
        quoted = true;
    }
    let mut in_quote = quoted;
    let mut equals = None;
    let mut end = start;
    while end < line.len() {
        let byte = line[end];
        if byte.is_ascii_whitespace() && !in_quote {
            break;
        }
        if equals.is_none() && byte == b'=' {
            equals = Some(end);
        }
        if byte == b'"' {
            in_quote = !in_quote;
        }
        end += 1;
    }
    let rest = &line[end..];
    let mut token = &line[start..end];
    if quoted && token.last() == Some(&b'"') {
        token = &token[..token.len() - 1];
    }
    match equals {
        None => (token, None, rest),
        Some(index) => {
            let split = index - start;
            let (name, mut value) = (&token[..split], &token[split + 1..]);
            if value.first() == Some(&b'"') {
                value = &value[1..];
                if value.last() == Some(&b'"') {
                    value = &value[..value.len() - 1];
                }
            }
            (name, Some(value), rest)
        }
    }
}

/// Linux `unknown_bootoption`：带 `.` 的模块参数忽略；`name=value` 进环境（同名覆盖），否则进
/// argv。
fn forward(
    parameters: &mut KernelParameters,
    name: &[u8],
    value: Option<&[u8]>,
) -> Result<(), CommandLineError> {
    if name.contains(&b'.') {
        return Ok(());
    }
    let Some(value) = value else {
        return push(
            &mut parameters.init_arguments,
            owned(name)?,
            CommandLineError::TooManyArguments,
        );
    };
    let mut entry = Vec::new();
    entry
        .try_reserve_exact(name.len() + 1 + value.len())
        .map_err(|_| CommandLineError::OutOfMemory)?;
    entry.extend_from_slice(name);
    entry.push(b'=');
    entry.extend_from_slice(value);
    if let Some(existing) = parameters
        .init_environment
        .iter_mut()
        .find(|existing| existing.starts_with(name) && existing.get(name.len()) == Some(&b'='))
    {
        *existing = entry;
        return Ok(());
    }
    push(
        &mut parameters.init_environment,
        entry,
        CommandLineError::TooManyEnvironment,
    )
}

/// 解析 kernel command line。
///
/// # Errors
///
/// 转交 init 的 argv/环境超过 32 项、`loglevel=` 非法或分配失败时返回对应错误。
pub(crate) fn parse(line: &[u8]) -> Result<KernelParameters, CommandLineError> {
    let mut parameters = KernelParameters::default();
    for entry in INITIAL_ENVIRONMENT {
        push(
            &mut parameters.init_environment,
            owned(entry)?,
            CommandLineError::TooManyEnvironment,
        )?;
    }
    let mut rest = line;
    loop {
        let skipped = rest
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .unwrap_or(rest.len());
        rest = &rest[skipped..];
        if rest.is_empty() {
            return Ok(parameters);
        }
        let (name, value, remaining) = next_argument(rest);
        rest = remaining;
        match (name, value) {
            // `--` 之后的全部词原样作为 init argv。
            (b"--", None) => loop {
                let skipped = rest
                    .iter()
                    .position(|byte| !byte.is_ascii_whitespace())
                    .unwrap_or(rest.len());
                rest = &rest[skipped..];
                if rest.is_empty() {
                    return Ok(parameters);
                }
                let end = rest
                    .iter()
                    .position(u8::is_ascii_whitespace)
                    .unwrap_or(rest.len());
                push(
                    &mut parameters.init_arguments,
                    owned(&rest[..end])?,
                    CommandLineError::TooManyArguments,
                )?;
                rest = &rest[end..];
            },
            (b"init", Some(path)) => parameters.init = Some(owned(path)?),
            (b"root", Some(device)) => parameters.root = Some(owned(device)?),
            (b"rootfstype", Some(kind)) => parameters.root_filesystem_type = Some(owned(kind)?),
            (b"ro", None) => parameters.read_only_root = true,
            (b"rw", None) => parameters.read_only_root = false,
            // 设备在 platform 装配时已同步发现，无需等待根设备出现。
            (b"rootwait", None) => {}
            (b"console", Some(device)) => {
                let name = device.split(|byte| *byte == b',').next().unwrap_or(device);
                parameters.console = Some(owned(name)?);
            }
            (b"loglevel", Some(level)) => {
                parameters.log_level = Some(
                    core::str::from_utf8(level)
                        .ok()
                        .and_then(|level| level.parse::<u8>().ok())
                        .ok_or(CommandLineError::InvalidLogLevel)?,
                );
            }
            // Linux `quiet` 等价 loglevel=4，`debug` 等价 loglevel=10。
            (b"quiet", None) => parameters.log_level = Some(4),
            (b"debug", None) => parameters.log_level = Some(10),
            (name, value) => forward(&mut parameters, name, value)?,
        }
    }
}
