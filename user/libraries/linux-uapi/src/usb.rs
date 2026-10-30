//! 标准 USB sysfs 设备发现；移除句柄在枚举时绑定 inode，不能用旧列表移除新连接。
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    sync::Arc,
};

/// 一次实际 USB 连接的快照；私有的写句柄只指向这次连接的 remove 属性。
#[derive(Clone)]
pub struct Device {
    pub name: String,
    pub location: String,
    pub vendor: String,
    pub product: String,
    /// 文件系统与 inode 的组合，重插后不可沿用旧 UI identity。
    pub key: (u64, u64),
    remove: Option<Arc<File>>,
}
impl PartialEq for Device {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.name == other.name
            && self.location == other.location
            && self.vendor == other.vendor
            && self.product == other.product
            && self.can_remove() == other.can_remove()
    }
}
impl Eq for Device {}
impl Device {
    /// 当前用户是否取得了该连接的移除权限。
    pub fn can_remove(&self) -> bool {
        self.remove.is_some()
    }
    /// 等待标准 remove 属性完成；仅成功返回后才可提示物理拔出。
    /// # Errors
    /// 无写权限、连接已替换/断开或控制器未确认停止时返回原生 I/O 错误。
    pub fn remove(&self) -> io::Result<()> {
        let mut file = self
            .remove
            .as_deref()
            .ok_or(io::ErrorKind::PermissionDenied)?;
        file.write_all(b"1\n")
    }
}
fn read(path: &Path, name: &str) -> io::Result<String> {
    fs::read_to_string(path.join(name)).map(|text| text.trim().to_owned())
}
fn inspect(path: &Path) -> io::Result<Device> {
    let before = fs::metadata(path)?;
    let key = (before.dev(), before.ino());
    let remove = OpenOptions::new()
        .write(true)
        .open(path.join("remove"))
        .ok();
    let device = Device {
        name: read(path, "product")?,
        location: path
            .file_name()
            .ok_or(io::ErrorKind::InvalidData)?
            .to_string_lossy()
            .into_owned(),
        vendor: read(path, "idVendor")?,
        product: read(path, "idProduct")?,
        key,
        remove: remove.map(Arc::new),
    };
    let after = fs::metadata(path)?;
    if (after.dev(), after.ino()) != key {
        return Err(io::ErrorKind::NotFound.into());
    }
    Ok(device)
}
/// 枚举标准 `/sys/bus/usb/devices`；不完整或在扫描期间断开的设备不发布。
/// # Errors
/// 目录读取失败返回 I/O 错误；系统没有 USB sysfs 时返回空列表。
pub fn devices() -> io::Result<Vec<Device>> {
    let entries = match fs::read_dir("/sys/bus/usb/devices") {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut devices = entries
        .filter_map(Result::ok)
        .filter_map(|entry| inspect(&entry.path()).ok())
        .collect::<Vec<_>>();
    devices.sort_by(|a, b| a.location.cmp(&b.location));
    Ok(devices)
}
