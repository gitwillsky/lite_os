//! 每个用户的 SMB2/3 FUSE 守护进程；认证输入只从 stdin 读取。
mod filesystem;
mod remote;
use fuser::{Config, MountOption, Session};
use network_drive::{Credentials, Mapping, Store};
use std::{
    env,
    io::{self, Read, Write},
    process::ExitCode,
    sync::Mutex,
};

fn read_credentials() -> io::Result<Credentials> {
    let mut input = String::new();
    let result = io::stdin()
        .take(65537)
        .read_to_string(&mut input)
        .and_then(|_| {
            if input.len() > 65536 {
                Err(io::ErrorKind::InvalidData.into())
            } else {
                Credentials::parse(&input)
            }
        });
    linux_uapi::credentials::erase(&mut input);
    result
}
fn letter(text: &str) -> io::Result<char> {
    let mut chars = text.trim_end_matches(':').chars();
    let value = chars
        .next()
        .ok_or(io::ErrorKind::InvalidInput)?
        .to_ascii_uppercase();
    if chars.next().is_some() || !('D'..='Z').contains(&value) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(value)
}
fn run() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["serve", source, point, policy] if matches!(*policy, "sign" | "encrypt") => {
            serve(source, point, *policy == "encrypt")
        }
        ["map", drive, source, policy, remember, reconnect]
            if matches!(*policy, "sign" | "encrypt")
                && matches!(*remember, "save" | "session")
                && matches!(*reconnect, "reconnect" | "once") =>
        {
            let mapping = Mapping::new(
                letter(drive)?,
                source,
                *reconnect == "reconnect",
                *policy == "encrypt",
            )?;
            Store::current()?.connect(&mapping, &read_credentials()?, *remember == "save")
        }
        ["disconnect", drive] => Store::current()?.disconnect(letter(drive)?),
        ["reconnect"] => {
            let failures = Store::current()?.reconnect()?;
            if failures.is_empty() {
                Ok(())
            } else {
                Err(io::Error::other(
                    failures
                        .into_iter()
                        .map(|(letter, error)| format!("{letter}: {error}"))
                        .collect::<Vec<_>>()
                        .join("；"),
                ))
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: smbfs serve SOURCE MOUNTPOINT sign|encrypt; map LETTER SOURCE sign|encrypt save|session reconnect|once (credentials on stdin); disconnect LETTER; reconnect",
        )),
    }
}
fn serve(source: &str, point: &str, encrypt: bool) -> io::Result<()> {
    let state = remote::State::new(source, read_credentials()?, encrypt)?;
    let (ready, initialized) = std::sync::mpsc::sync_channel(1);
    let fs = filesystem::SmbFilesystem {
        state: Mutex::new(state),
        ready,
    };
    let mut config = Config::default();
    // UNC 可含逗号，不能将用户文本拼进 helper 的 -o 选项；源地址唯一保存在映射记录与 SMB 会话。
    config.mount_options = vec![
        MountOption::FSName("SMB".into()),
        MountOption::Subtype("smbfs".into()),
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    let session = Session::new(fs, point, &config)?.spawn()?;
    // 挂载和认证均成功后才发布就绪；父进程据此决定是否保留映射记录。
    initialized
        .recv_timeout(std::time::Duration::from_secs(15))
        .map_err(io::Error::other)?;
    println!("READY");
    io::stdout().flush()?;
    session.join()
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            println!("网络驱动器：{error}");
            ExitCode::FAILURE
        }
    }
}
