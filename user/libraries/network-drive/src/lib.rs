//! 每用户 SMB 映射与凭据的唯一 owner；UI、登录重连与守护进程共用，不把密码写进 argv/env。
mod mount;
mod store;
use std::io;
pub use store::{Credentials, Mapping, Store};

/// Windows UNC 的规范化形式；拒绝凭据 URL、路径遍历与多余的子目录。
/// `input` 接受 \\server\share 或 //server/share，返回无密码的 //server/share。
/// 地址/共享名不完整、含控制字符或 .. 时返回 InvalidInput。
pub fn normalize_source(input: &str) -> io::Result<String> {
    let input = input.trim().replace('\\', "/");
    let fields: Vec<_> = input.strip_prefix("//").unwrap_or("").split('/').collect();
    if fields.len() != 2
        || fields
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
        || input.chars().any(char::is_control)
        || input.contains('@')
        || input.contains('?')
        || input.contains('#')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "请输入 \\\\服务器\\共享名",
        ));
    }
    let server = fields[0];
    if let Some((host, port)) = server.rsplit_once(':')
        && (host.is_empty()
            || host.contains(':')
            || port.parse::<u16>().ok().is_none_or(|port| port == 0))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(format!("//{}/{}", fields[0], fields[1]))
}
