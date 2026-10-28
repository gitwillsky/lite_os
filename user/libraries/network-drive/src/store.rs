//! 同一用户的映射记录与 mount.cifs credentials 文件；所有写入均在 0700 目录内原子替换。
use std::{
    env,
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// 不含密码的映射记录；source 始终为规范化 UNC。
#[derive(Clone, Debug)]
pub struct Mapping {
    pub letter: char,
    pub source: String,
    pub reconnect: bool,
    pub encrypt: bool,
}
impl Mapping {
    /// 校验记录并构造；letter 为 D..Z，source 为完整共享 UNC。
    ///
    /// # Parameters
    /// - `letter`：D..Z 的盘符，输入转为大写。
    /// - `source`：完整的服务器/共享 UNC，可包含显式高端口。
    /// - `reconnect`：登录时自动恢复，连接时必须同时保存凭据。
    /// - `encrypt`：要求 SMB3 加密，不能降为仅签名。
    /// # Returns
    /// 规范化的映射记录。
    /// # Errors
    /// 格式错误返回 InvalidInput，不访问文件系统。
    pub fn new(letter: char, source: &str, reconnect: bool, encrypt: bool) -> io::Result<Self> {
        let letter = letter.to_ascii_uppercase();
        if !('D'..='Z').contains(&letter) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(Self {
            letter,
            source: crate::normalize_source(source)?,
            reconnect,
            encrypt,
        })
    }
}

/// 只经私有文件或 stdin 传递的认证输入；不实现 Debug/Display，避免错误日志泄露密码。
pub struct Credentials {
    pub username: String,
    pub password: String,
    pub domain: String,
}
impl Credentials {
    /// 解析 mount.cifs 的逐行 credentials 格式；未知/重复字段、缺少用户名或嵌入控制字符返回 InvalidData。
    pub fn parse(text: &str) -> io::Result<Self> {
        let mut username = None;
        let mut password = None;
        let mut domain = None;
        for line in text.lines() {
            let (key, value) = line.split_once('=').ok_or(io::ErrorKind::InvalidData)?;
            let slot = match key {
                "username" => &mut username,
                "password" => &mut password,
                "domain" => &mut domain,
                _ => return Err(io::ErrorKind::InvalidData.into()),
            };
            if slot.replace(value).is_some() {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        let value = Self {
            username: username.ok_or(io::ErrorKind::InvalidData)?.to_owned(),
            password: password.ok_or(io::ErrorKind::InvalidData)?.to_owned(),
            domain: domain.unwrap_or_default().to_owned(),
        };
        value.validate()?;
        Ok(value)
    }
    /// 编码 credentials 文件；空用户名或 CR/LF/NUL 返回 InvalidInput，不修改文件。
    pub fn encode(&self) -> io::Result<String> {
        self.validate()?;
        Ok(format!(
            "username={}\npassword={}\ndomain={}\n",
            self.username, self.password, self.domain
        ))
    }
    fn validate(&self) -> io::Result<()> {
        if self.username.len() + self.password.len() + self.domain.len() + 28 > 65536
            || self.username.is_empty()
            || [&self.username, &self.password, &self.domain]
                .iter()
                .any(|text| text.chars().any(|c| matches!(c, '\n' | '\r' | '\0')))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "用户名不能为空，凭据不能包含换行或 NUL",
            ));
        }
        Ok(())
    }
}

/// 当前用户的唯一记录目录与实际挂载目录；不创建 root 代挂服务。
#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
    root: PathBuf,
}
impl Store {
    /// 按实际 HOME/XDG_CONFIG_HOME 建立私有目录；缺少 HOME、目录为符号链接或 I/O 失败返回错误。
    pub fn current() -> io::Result<Self> {
        let home = PathBuf::from(env::var_os("HOME").ok_or(io::ErrorKind::NotFound)?);
        if !home.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let config = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let store = Self {
            directory: config.join("liteos/network-drives"),
            root: home.join("Network"),
        };
        private_directory(&store.directory)?;
        private_directory(&store.root)?;
        Ok(store)
    }
    /// 某映射的真实普通路径。letter 不在 D..Z 时返回 InvalidInput。
    pub fn mountpoint(&self, letter: char) -> io::Result<PathBuf> {
        if !('D'..='Z').contains(&letter) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(self.root.join(letter.to_string()))
    }
    pub(crate) fn lock(&self) -> io::Result<fs::File> {
        // 每用户同一把 OFD flock 串行化多窗口连接/断开；进程退出自动释放，不留过期 PID 锁。
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.directory.join(".lock"))?;
        file.lock()?;
        Ok(file)
    }
    fn config_path(&self, letter: char) -> io::Result<PathBuf> {
        self.mountpoint(letter)?;
        Ok(self.directory.join(format!("{letter}.conf")))
    }
    fn credential_path(&self, letter: char) -> io::Result<PathBuf> {
        self.mountpoint(letter)?;
        Ok(self.directory.join(format!("{letter}.credentials")))
    }
    /// 读取全部记录，返回按盘符排序的列表；损坏记录返回错误，不能静默丢失已保存映射。
    pub fn list(&self) -> io::Result<Vec<Mapping>> {
        let mut result = Vec::new();
        for letter in 'D'..='Z' {
            let text = match fs::read_to_string(self.config_path(letter)?) {
                Ok(text) => text,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let fields: Vec<_> = text.lines().collect();
            if fields.len() != 3
                || !matches!(fields[1], "reconnect=0" | "reconnect=1")
                || !matches!(fields[2], "encrypt=0" | "encrypt=1")
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "网络驱动器记录损坏",
                ));
            }
            result.push(Mapping::new(
                letter,
                fields[0]
                    .strip_prefix("source=")
                    .ok_or(io::ErrorKind::InvalidData)?,
                fields[1].ends_with('1'),
                fields[2].ends_with('1'),
            )?);
        }
        Ok(result)
    }
    /// 提交成功挂载的配置；登录重连必须显式保存凭据。取消保存会删除原凭据，所有错误原样返回。
    pub(crate) fn save(
        &self,
        mapping: &Mapping,
        credentials: &Credentials,
        remember: bool,
    ) -> io::Result<()> {
        if mapping.reconnect && !remember {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "登录重连需要保存凭据",
            ));
        }
        let path = self.credential_path(mapping.letter)?;
        if remember {
            let mut text = credentials.encode()?;
            let result = atomic_private(&path, text.as_bytes());
            linux_uapi::credentials::erase(&mut text);
            result?;
        } else {
            remove_optional(&path)?;
        }
        let text = format!(
            "source={}\nreconnect={}\nencrypt={}\n",
            mapping.source,
            u8::from(mapping.reconnect),
            u8::from(mapping.encrypt)
        );
        let result = atomic_private(&self.config_path(mapping.letter)?, text.as_bytes());
        if result.is_err() {
            remove_optional(&path)?;
        }
        result
    }
    /// 读取保存的私有凭据；文件为符号链接/非普通文件/权限过宽、超长或字段非法时拒绝。
    pub fn credentials(&self, letter: char) -> io::Result<Credentials> {
        let path = self.credential_path(letter)?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut text = String::new();
        let result = (|| {
            fs::File::open(path)?
                .take(65537)
                .read_to_string(&mut text)?;
            if text.len() > 65536 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            Credentials::parse(&text)
        })();
        linux_uapi::credentials::erase(&mut text);
        result
    }
    /// 成功断开后撤销记录和凭据；未挂载记录也可删除，不遗留自动重连的密码文件。
    pub fn remove(&self, letter: char) -> io::Result<()> {
        remove_optional(&self.config_path(letter)?)?;
        remove_optional(&self.credential_path(letter)?)?;
        match fs::remove_dir(self.mountpoint(letter)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}
fn remove_optional(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
fn private_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}
fn atomic_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // 同目录 create_new 防止覆盖已有临时文件；PID + 原子序号只命名，不携带凭据。
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        fs::File::open(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

impl Drop for Credentials {
    fn drop(&mut self) {
        linux_uapi::credentials::erase(&mut self.password);
    }
}
