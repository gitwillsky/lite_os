//! 启动/断开标准用户态 FUSE 挂载；真实状态只从 /proc/mounts 读取，记录不伪装成在线连接。
use crate::{Credentials, Mapping, Store};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

impl Store {
    /// 查询实际 mount table 中的该盘符；只有匹配 fuse.smbfs 的实际挂载才为 true。
    pub fn mounted(&self, letter: char) -> io::Result<bool> {
        let point = self.mountpoint(letter)?;
        Ok(fs::read_to_string("/proc/mounts")?.lines().any(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.len() >= 3
                && fields[2] == "fuse.smbfs"
                && unescape(fields[1]) == point.as_os_str().to_string_lossy()
        }))
    }
    /// 连接新映射并提交配置；凭据仅进入 child stdin。失败撤销挂载与新记录，不重放写请求。
    pub fn connect(
        &self,
        mapping: &Mapping,
        credentials: &Credentials,
        remember: bool,
    ) -> io::Result<()> {
        let _lock = self.lock()?;
        if self.list()?.iter().any(|old| old.letter == mapping.letter)
            || self.mounted(mapping.letter)?
        {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        if let Err(error) = self.save(mapping, credentials, remember) {
            return match self.remove(mapping.letter) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::other(format!(
                    "{error}；撤销记录失败：{cleanup}"
                ))),
            };
        }
        if let Err(error) = self.launch(mapping, credentials) {
            let cleanup = self.disconnect_locked(mapping.letter);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::other(format!("{error}；撤销失败：{cleanup}"))),
            };
        }
        Ok(())
    }
    /// 按标准 fusermount3 -u 断开实际挂载，再删除凭据/重连记录；卸载失败保留记录供用户重试。
    pub fn disconnect(&self, letter: char) -> io::Result<()> {
        let _lock = self.lock()?;
        self.disconnect_locked(letter)
    }
    fn disconnect_locked(&self, letter: char) -> io::Result<()> {
        if self.mounted(letter)? {
            let status = Command::new("/bin/fusermount3")
                .args(["-u", "--"])
                .arg(self.mountpoint(letter)?)
                .status()?;
            if !status.success() {
                return Err(io::Error::other("网络驱动器正被使用，无法断开"));
            }
        }
        self.remove(letter)
    }
    /// 登录后恢复显式保存的映射；逐项返回错误，不阻塞桌面线程，不重试无密码的映射。
    pub fn reconnect(&self) -> io::Result<Vec<(char, io::Error)>> {
        let _lock = self.lock()?;
        let mut failures = Vec::new();
        for mapping in self.list()?.into_iter().filter(|mapping| mapping.reconnect) {
            let result = self.restore(&mapping);
            if let Err(error) = result {
                failures.push((mapping.letter, error));
            }
        }
        Ok(failures)
    }
    /// 双击映射盘时恢复该连接，返回真实挂载路径；未保存密码或服务器不可达时返回明确错误。
    pub fn activate(&self, letter: char) -> io::Result<std::path::PathBuf> {
        let _lock = self.lock()?;
        let mapping = self
            .list()?
            .into_iter()
            .find(|mapping| mapping.letter == letter)
            .ok_or(io::ErrorKind::NotFound)?;
        self.restore(&mapping)?;
        self.mountpoint(letter)
    }
    fn restore(&self, mapping: &Mapping) -> io::Result<()> {
        if self.mounted(mapping.letter)? {
            match fs::metadata(self.mountpoint(mapping.letter)?) {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotConnected => {
                    let status = Command::new("/bin/fusermount3")
                        .args(["-u", "--"])
                        .arg(self.mountpoint(mapping.letter)?)
                        .status()?;
                    if !status.success() {
                        return Err(io::Error::other("无法卸载已失联的网络驱动器"));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let credentials = self.credentials(mapping.letter).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(error.kind(), "此映射未保存密码，请先断开后重新映射")
            } else {
                error
            }
        })?;
        if let Err(error) = self.launch(mapping, &credentials) {
            if self.mounted(mapping.letter)? {
                let status = Command::new("/bin/fusermount3")
                    .args(["-u", "--"])
                    .arg(self.mountpoint(mapping.letter)?)
                    .status()?;
                if !status.success() {
                    return Err(io::Error::other(format!("{error}；无法卸载失败的连接")));
                }
            }
            return Err(error);
        }
        Ok(())
    }
    fn launch(&self, mapping: &Mapping, credentials: &Credentials) -> io::Result<()> {
        let point = self.mountpoint(mapping.letter)?;
        fs::create_dir_all(&point)?;
        if !fs::symlink_metadata(&point)?.is_dir() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        if fs::read_dir(&point)?.next().is_some() {
            return Err(io::Error::other("挂载目录不是空目录"));
        }
        let mut input = credentials.encode()?;
        if input.len() > 65536 {
            linux_uapi::credentials::erase(&mut input);
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let child = Command::new("/bin/smbfs")
            .arg("serve")
            .arg(&mapping.source)
            .arg(&point)
            .arg(if mapping.encrypt { "encrypt" } else { "sign" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                linux_uapi::credentials::erase(&mut input);
                return Err(error);
            }
        };
        if let Err(error) = child
            .stdin
            .take()
            .ok_or(io::ErrorKind::BrokenPipe)?
            .write_all(input.as_bytes())
        {
            linux_uapi::credentials::erase(&mut input);
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        linux_uapi::credentials::erase(&mut input);
        let stdout = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout)
                .take(4096)
                .read_line(&mut line)
                .map(|_| line);
            let _ = sender.send(result);
        });
        match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(line)) if line.trim() == "READY" => {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(())
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                let message = match result {
                    Ok(Ok(line)) if !line.trim().is_empty() => line.trim().to_string(),
                    _ => "连接共享失败或超时".into(),
                };
                Err(io::Error::other(message))
            }
        }
    }
}
fn unescape(input: &str) -> String {
    let mut result = Vec::new();
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1] <= b'3'
            && bytes[index + 1..index + 4]
                .iter()
                .all(|byte| (b'0'..=b'7').contains(byte))
        {
            result.push(
                (bytes[index + 1] - b'0') * 64 + (bytes[index + 2] - b'0') * 8 + bytes[index + 3]
                    - b'0',
            );
            index += 4;
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}
