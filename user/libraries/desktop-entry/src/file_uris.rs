//! 本地文件 URI 用于桌面文件传输（RFC 8089 与 text/uri-list）。
use std::{
    io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
};

/// 文件 URI 列表的字节上限。
pub const MAX_BYTES: usize = 128 * 1024;
/// 标准 URI 列表 MIME。
pub const URI_LIST: &str = "text/uri-list";
/// 文件管理器剪贴板格式，首行为 cut 或 copy，其后为本地文件 URI。
pub const COPIED_FILES: &str = "x-special/gnome-copied-files";
/// 文件选择格式的读取顺序。优先保留剪切语义。
pub const SELECTION_TYPES: [&str; 2] = [COPIED_FILES, URI_LIST];

/// 解码文件剪贴板 `mime` 数据。标准 URI 列表表示复制，文件管理器格式可请求移动。
/// 格式、路径或数量不合法时返回错误。
pub fn decode_selection(
    mime: &str,
    bytes: &[u8],
    max_paths: usize,
) -> io::Result<(bool, Vec<PathBuf>)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "剪贴板不包含有效的本地文件列表");
    let (cut, uris) = match mime {
        COPIED_FILES => {
            let separator = bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .ok_or_else(invalid)?;
            let (action, uris) = (&bytes[..separator], &bytes[separator + 1..]);
            match action {
                b"cut" => (true, uris),
                b"copy" => (false, uris),
                _ => return Err(invalid()),
            }
        }
        URI_LIST => (false, bytes),
        _ => return Err(invalid()),
    };
    Ok((cut, decode(uris, max_paths).ok_or_else(invalid)?))
}

/// 编码文件剪贴板的两种 MIME 数据。`cut` 为真时请求移动，标准 URI 列表仍可供其他客户端读取。
/// 路径无效或超过 `max_paths`、字节上限时返回错误。
pub fn encode_selection(
    paths: &[PathBuf],
    cut: bool,
    max_paths: usize,
) -> io::Result<[(&'static str, String); 2]> {
    let uris = encode(paths, max_paths)?;
    let action = if cut { "cut" } else { "copy" };
    let lines = uris.trim_end_matches("\r\n").replace("\r\n", "\n");
    Ok([
        (COPIED_FILES, format!("{action}\n{lines}")),
        (URI_LIST, uris),
    ])
}

fn local_path(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name().is_some()
        && !path.as_os_str().as_bytes().starts_with(b"//")
        && !path.as_os_str().as_bytes().contains(&0)
        && !path.components().any(|part| part == Component::ParentDir)
}

/// 将本地绝对路径编码成 URI 列表，路径非法、超出 `max_paths` 或字节上限时返回错误。
pub fn encode(paths: &[PathBuf], max_paths: usize) -> io::Result<String> {
    if paths.is_empty() || paths.len() > max_paths {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut result = String::new();
    for path in paths {
        if !local_path(path) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        result.push_str("file://");
        for byte in path.as_os_str().as_bytes() {
            if byte.is_ascii_alphanumeric() || b"/-._~".contains(byte) {
                result.push(char::from(*byte));
            } else {
                use std::fmt::Write;
                write!(result, "%{byte:02X}").expect("writing to a string");
            }
            if result.len() > MAX_BYTES {
                return Err(io::ErrorKind::InvalidInput.into());
            }
        }
        result.push_str("\r\n");
        if result.len() > MAX_BYTES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
    }
    Ok(result)
}

/// 解码最多 `max_paths` 项的本地 URI 列表，拒绝非本地、空列表或损坏的路径。
pub fn decode(bytes: &[u8], max_paths: usize) -> Option<Vec<PathBuf>> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let mut paths = Vec::new();
    for line in std::str::from_utf8(bytes).ok()?.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rest = line.strip_prefix("file://")?;
        let path = rest
            .strip_prefix("localhost/")
            .map_or(rest, |_| &rest["localhost".len()..]);
        if !path.starts_with('/') || path.starts_with("//") || paths.len() >= max_paths {
            return None;
        }
        let bytes = path.as_bytes();
        let mut native = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte == b'%' {
                let code = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
                let value = u8::from_str_radix(code, 16).ok()?;
                if value == 0 || value == b'/' {
                    return None;
                }
                native.push(value);
                index += 3;
            } else {
                if byte.is_ascii_control() || matches!(byte, b'#' | b'?' | b' ') {
                    return None;
                }
                native.push(byte);
                index += 1;
            }
        }
        let path = PathBuf::from(std::ffi::OsString::from_vec(native));
        if !local_path(&path) {
            return None;
        }
        paths.push(path);
    }
    (!paths.is_empty()).then_some(paths)
}
