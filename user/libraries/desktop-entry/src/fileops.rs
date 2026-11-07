//! 文件操作：失败只清理本次创建的条目，命名冲突由原子创建/移动裁决。
use crate::trash::{self, Volume};
use linux_uapi::filesystem::{mount_path, rename_noreplace};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn info_path(volume: &Volume, path: &Path) -> io::Result<PathBuf> {
    let mut name = file_name(path)?.to_os_string();
    name.push(".trashinfo");
    Ok(volume.info().join(name))
}
fn file_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "路径没有可用的名称"))
}
fn numbered(name: &OsStr, number: u64) -> OsString {
    if number == 1 {
        return name.to_owned();
    }
    suffixed(name, &format!(" ({number})"), usize::MAX)
}
fn suffixed(name: &OsStr, suffix: &str, limit: usize) -> OsString {
    let bytes = name.as_bytes();
    let split = bytes
        .iter()
        .rposition(|byte| *byte == b'.')
        .filter(|index| *index != 0)
        .unwrap_or(bytes.len());
    let budget = limit.saturating_sub(suffix.len());
    let (stem_end, extension_end) = if bytes.len() <= budget {
        (split, bytes.len())
    } else {
        let text = name.to_str();
        let boundary = |index| text.map_or(index, |text| text.floor_char_boundary(index));
        // 尽量保留扩展名，但至少为原名称的首字符留位置。
        let first = text
            .and_then(|text| text.chars().next())
            .map_or(1, char::len_utf8);
        let extension = (bytes.len() - split).min(budget.saturating_sub(first));
        let mut end = boundary(split + extension);
        if end == split + 1 {
            end = split; // 截断后只剩分隔点时不保留末尾点。
        }
        (boundary(split.min(budget - (end - split))), end)
    };
    let mut name = OsString::with_capacity(stem_end + suffix.len() + extension_end - split);
    name.push(OsStr::from_bytes(&bytes[..stem_end]));
    name.push(suffix);
    name.push(OsStr::from_bytes(&bytes[split..extension_end]));
    name
}
fn absent(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}
fn free_name(dir: &Path, name: &OsStr, suffix: &str) -> io::Result<PathBuf> {
    let mut limit = usize::MAX;
    for number in 1..=u64::MAX {
        let suffix = if number == 1 {
            Cow::Borrowed(suffix)
        } else {
            Cow::Owned(format!("{suffix} ({number})"))
        };
        loop {
            let candidate = if suffix.is_empty() {
                dir.join(name)
            } else {
                dir.join(suffixed(name, &suffix, limit))
            };
            match absent(&candidate) {
                Ok(true) => return Ok(candidate),
                Ok(false) => break,
                // 原名已通过该目录的查询，超长只在追加后缀后出现。用原名
                // 字节数作已知可用的上限，后续编号沿用该上限，不再反复探测。
                Err(error)
                    if error.kind() == io::ErrorKind::InvalidFilename
                        && !suffix.is_empty()
                        && limit == usize::MAX
                        && name.len() > suffix.len() =>
                {
                    limit = name.len();
                }
                Err(error) => return Err(error),
            }
        }
    }
    Err(io::Error::other("文件名序号耗尽"))
}

/// 路径是否位于当前用户的某个回收站。
pub fn in_trash(home: &Path, path: &Path) -> bool {
    trash_directories(&trash::volumes(home)).any(|directory| path.starts_with(directory))
}

fn trash_directories(volumes: &[Volume]) -> impl Iterator<Item = PathBuf> + '_ {
    volumes.iter().flat_map(|volume| {
        let files = volume.files();
        let canonical = fs::canonicalize(&files).ok().filter(|path| *path != files);
        std::iter::once(files).chain(canonical)
    })
}
fn encode_path(path: &Path) -> String {
    let mut encoded = String::new();
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.!~*'()".contains(byte) {
            encoded.push(*byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
fn decode_path(value: &str) -> io::Result<PathBuf> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "回收站路径编码损坏"))?;
            index += 3;
            hex
        } else {
            let byte = bytes[index];
            index += 1;
            byte
        };
        if byte == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "回收站路径包含 NUL",
            ));
        }
        decoded.push(byte);
    }
    Ok(PathBuf::from(OsString::from_vec(decoded)))
}

/// 回收站根条目的 Trash 1.0 信息：原路径与删除时刻。
pub struct TrashInfo {
    pub original: PathBuf,
    /// 删除时刻（Unix 秒）；`DeletionDate` 缺失或格式不符时为 `None`，不影响还原。
    pub deleted: Option<i64>,
}

/// 读取回收站根条目 `path` 的信息文件。
///
/// # Errors
///
/// 信息文件不可读、缺少 `[Trash Info]` 头或 `Path=`，或路径编码损坏。
pub fn trash_info_of(volume: &Volume, path: &Path) -> io::Result<TrashInfo> {
    let info = info_path(volume, path)?;
    if !fs::symlink_metadata(&info)?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let text = fs::read_to_string(info)?;
    if text.lines().next() != Some("[Trash Info]") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "回收站信息文件损坏",
        ));
    }
    let value = |key: &str| text.lines().skip(1).find_map(|line| line.strip_prefix(key));
    let original = value("Path=")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "回收站缺少原路径"))?;
    let original = decode_path(original)?;
    if original.file_name().is_none()
        || original
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        || (volume.top().is_some() && original.is_absolute())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "回收站原路径无效",
        ));
    }
    Ok(TrashInfo {
        original: volume.base().join(original),
        deleted: value("DeletionDate=").and_then(deletion_date),
    })
}

/// Trash 1.0 的 `DeletionDate`（本地时间 `YYYY-MM-DDThh:mm:ss`）换成 Unix 秒。
fn deletion_date(value: &str) -> Option<i64> {
    let (date, time) = value.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.splitn(3, ':').map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&month)
        || !(1..=i64::from(calendar::days_in_month(year, month as u32))).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let days = calendar::days_from_civil(calendar::Date {
        year,
        month: month as u32,
        day: day as u32,
    });
    Some(calendar::zone::from_local(
        days * 86_400 + hour * 3600 + minute * 60 + second,
    ))
}

/// 删除确认与执行共用的处置方式；永久删除必须先由 UI 明确确认。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// 原子移入当前用户的同卷回收站。
    Trash,
    /// 不经过本地回收站；包括网络盘等跨文件系统条目。
    Permanent,
}

/// 删除入口指定的意图。拖入回收站时必须能回收，Shift+Delete 则明确要求永久删除。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeleteIntent {
    Default,
    RecycleOnly,
    Permanent,
}

/// 确认框展示的项目与处置方式，以及确认前看到的文件身份。
pub struct Deletion {
    path: PathBuf,
    pub mode: DeleteMode,
    directory: bool,
    identity: (u64, u64),
    original_name: Option<OsString>,
}

/// 进入原位编辑时看到的文件条目，提交前以同一身份复核。
pub struct Rename {
    path: PathBuf,
    identity: (u64, u64),
}

impl Rename {
    /// 记录原路径本身的身份，符号链接不跟随目标。
    /// # Errors
    /// 项目已移动或不可读取时返回 I/O 错误。
    pub fn prepare(path: PathBuf) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            identity: (metadata.dev(), metadata.ino()),
        })
    }

    /// 正在编辑的原始路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 复核原路径仍指向进入编辑时的条目。
    /// # Errors
    /// 项目已消失或身份不同返回 NotFound，其他 I/O 错误原样返回。
    pub fn verify(&self) -> io::Result<()> {
        let changed =
            || io::Error::new(io::ErrorKind::NotFound, "项目已移动或被替换，请重新选择。");
        let current = fs::symlink_metadata(&self.path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                changed()
            } else {
                error
            }
        })?;
        if self.identity != (current.dev(), current.ino()) {
            return Err(changed());
        }
        Ok(())
    }

    /// 复核原条目后按给定名称在同一目录改名，返回新路径。
    /// # Errors
    /// 项目已消失或身份不同返回 NotFound。名称无效、冲突及其他 I/O 错误原样返回。
    pub fn commit(&self, name: &str) -> io::Result<PathBuf> {
        self.verify()?;
        rename(&self.path, name)
    }
}

/// A completed move and the identity expected at its new path.
pub struct UndoItem {
    previous: PathBuf,
    current: PathBuf,
    identity: (u64, u64),
}

/// One user action. Entries still waiting for undo stay here after a partial failure.
pub enum Undo {
    Rename(UndoItem),
    Delete(Vec<UndoItem>),
}

impl Undo {
    /// 返回与操作种类对应的资源管理器菜单文案。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Rename(_) => "撤消重命名(U)",
            Self::Delete(_) => "撤消删除(U)",
        }
    }

    /// 提交同目录改名草稿，返回目标路径和撤消记录。错误同 Rename::commit。
    pub fn rename(draft: &Rename, name: &str) -> io::Result<(PathBuf, Self)> {
        let current = draft.commit(name)?;
        let record = Self::Rename(UndoItem {
            previous: draft.path.clone(),
            current: current.clone(),
            identity: draft.identity,
        });
        Ok((current, record))
    }

    /// Restore exact previous names. The preflight prevents a known conflict in one
    /// entry from leaving the rest of a group half restored.
    pub fn reverse(&mut self, home: &Path) -> io::Result<Option<PathBuf>> {
        let verify = |item: &UndoItem| -> io::Result<()> {
            if !item.matches(&item.current) {
                return Err(io::Error::other("要撤消的项目已移动、替换或不可访问。"));
            }
            if !absent(&item.previous)? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "原位置已有同名项目。",
                ));
            }
            Ok(())
        };
        match self {
            Self::Rename(item) => {
                verify(item)?;
                rename_noreplace(&item.current, &item.previous)?;
                Ok(Some(item.previous.clone()))
            }
            Self::Delete(items) => {
                for item in items.iter() {
                    verify(item)?;
                }
                let mut last = None;
                while let Some(item) = items.last() {
                    match restore_undo(home, &item.current, &item.previous) {
                        Ok(path) => {
                            items.pop();
                            last = Some(path);
                        }
                        Err(error) => {
                            // The move can succeed even if removing its metadata fails.
                            if item.matches(&item.previous) && !item.matches(&item.current) {
                                items.pop();
                            }
                            return Err(error);
                        }
                    }
                }
                Ok(last)
            }
        }
    }

    /// 返回一组删除撤消是否已无剩余条目。
    pub fn exhausted(&self) -> bool {
        matches!(self, Self::Delete(items) if items.is_empty())
    }
}

/// 将 `path` 改为同一目录中的 `name`，返回目标路径。
/// # Errors
/// 经典外壳不可用的名称返回 InvalidInput；已存在的目标返回 AlreadyExists。文件系统原子裁决冲突。
fn rename(path: &Path, name: &str) -> io::Result<PathBuf> {
    // Linux 应用可能创建过经典外壳不能新建的名称。原样结束编辑仍视为无操作。
    if path.file_name() == Some(OsStr::new(name)) {
        return Ok(path.to_owned());
    }
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
    if name.is_empty() || name.ends_with([' ', '.']) {
        return Err(invalid("名称不能为空，也不能以空格或句点结尾。"));
    }
    if name.contains(|ch: char| ch < ' ' || "<>:\"/\\|?*".contains(ch)) {
        return Err(invalid(
            "名称不能包含控制字符或下列字符：< > : \" / \\ | ? *",
        ));
    }
    let device = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || device
            .strip_prefix("COM")
            .or_else(|| device.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(
                    number,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
    {
        return Err(invalid("不能使用系统保留的设备名称。"));
    }
    let target = path.with_file_name(name);
    rename_noreplace(path, &target).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("已存在名为“{name}”的文件或文件夹。"),
            )
        } else {
            error
        }
    })?;
    Ok(target)
}

impl UndoItem {
    fn matches(&self, path: &Path) -> bool {
        fs::symlink_metadata(path)
            .is_ok_and(|metadata| self.identity == (metadata.dev(), metadata.ino()))
    }
}

impl Deletion {
    /// 确认时该条目是否为文件夹；符号链接按链接本身处理。
    pub fn is_directory(&self) -> bool {
        self.directory
    }

    /// 确认框使用的名称；从回收站永久删除时优先显示原名。
    pub fn name(&self) -> &OsStr {
        self.original_name
            .as_deref()
            .unwrap_or_else(|| self.path.file_name().expect("prepared deletion has a name"))
    }

    /// 在确认前捕获 `home` 可见回收站的根条目，返回供 [`Self::commit`] 复核的永久删除清单。
    ///
    /// # Errors
    /// 已存在的回收目录或条目无法读取时返回 I/O 错误，尚未删除任何文件。
    pub fn prepare_empty_trash(home: &Path) -> io::Result<Vec<Self>> {
        let mut entries = Vec::new();
        visit_trash(&trash::volumes(home), |_, entry| {
            let metadata = entry.metadata()?;
            entries.push(Self {
                path: entry.path(),
                mode: DeleteMode::Permanent,
                directory: metadata.is_dir(),
                identity: (metadata.dev(), metadata.ino()),
                original_name: None,
            });
            Ok(())
        })?;
        Ok(entries)
    }

    /// 根据 `intent` 准备 `paths` 的处置清单。路径不可读取或无法满足回收要求时返回错误。
    pub fn prepare(
        home: &Path,
        paths: Vec<PathBuf>,
        intent: DeleteIntent,
    ) -> io::Result<Vec<Self>> {
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        let single = paths.len() == 1;
        let volumes = if single || (!paths.is_empty() && intent != DeleteIntent::Permanent) {
            trash::volumes(home)
        } else {
            Vec::new()
        };
        let recycle_directories: Vec<_> = if intent == DeleteIntent::Permanent {
            Vec::new()
        } else {
            trash_directories(&volumes).collect()
        };
        // 只为确认清单复用目录解析与回收卷判断。确认后按现有路径重新验证，
        // 避免文件、权限或挂载在等待用户确认期间变化。
        let mut parents = BTreeMap::<&Path, (PathBuf, Option<DeleteMode>)>::new();
        for path in &paths {
            let input_parent = path.parent().unwrap_or_else(|| Path::new("/"));
            let (parent, parent_mode) = match parents.entry(input_parent) {
                Entry::Occupied(parent) => parent.into_mut(),
                Entry::Vacant(parent) => parent.insert((fs::canonicalize(input_parent)?, None)),
            };
            let path = parent.join(file_name(path)?);
            if !single && !seen.insert(path.clone()) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            let mode = if intent == DeleteIntent::Permanent
                || recycle_directories
                    .iter()
                    .any(|directory| path.starts_with(directory))
            {
                DeleteMode::Permanent
            } else if let Some(mode) = *parent_mode {
                mode
            } else {
                let mode = delete_mode_for_parent(home, parent)?;
                *parent_mode = Some(mode);
                mode
            };
            if intent == DeleteIntent::RecycleOnly && mode != DeleteMode::Trash {
                return Err(io::Error::other(format!(
                    "无法将“{}”移入本地回收站。",
                    path.display()
                )));
            }
            let original_name = if single && mode == DeleteMode::Permanent {
                containing_trash(&volumes, &path)
                    .and_then(|volume| trash_info_of(volume, &path).ok())
                    .and_then(|info| info.original.file_name().map(OsStr::to_owned))
            } else {
                None
            };
            entries.push(Self {
                path,
                mode,
                directory: metadata.is_dir(),
                identity: (metadata.dev(), metadata.ino()),
                original_name,
            });
        }
        Ok(entries)
    }

    /// 执行已确认的清单。先验证所有项目的身份，任一项目变化则在执行前返回错误。
    /// 执行阶段遇到 I/O 错误时停止，并保留已完成的操作。
    pub fn commit(
        home: &Path,
        entries: Vec<Self>,
        mut completed: impl FnMut(Option<UndoItem>),
    ) -> io::Result<()> {
        for entry in &entries {
            let current = fs::symlink_metadata(&entry.path)?;
            if entry.identity != (current.dev(), current.ino()) {
                return Err(io::Error::other("项目在确认期间已更改，请重新选择。"));
            }
        }
        for (done, entry) in entries.into_iter().enumerate() {
            let current = delete(home, &entry.path, entry.mode).map_err(|error| {
                let name = entry.name().to_string_lossy();
                let progress = if done == 0 {
                    String::new()
                } else {
                    format!("\n已处理 {done} 项。")
                };
                io::Error::new(error.kind(), format!("无法删除“{name}”：{error}{progress}"))
            })?;
            completed(current.map(|current| UndoItem {
                previous: entry.path,
                current,
                identity: entry.identity,
            }));
        }
        Ok(())
    }
}

/// 判断是否可原子移入同一文件系统的回收站；此阶段不创建目录。
pub fn delete_mode(home: &Path, path: &Path) -> io::Result<DeleteMode> {
    if in_trash(home, path) {
        return Ok(DeleteMode::Permanent);
    }
    let parent = fs::canonicalize(path.parent().unwrap_or_else(|| Path::new("/")))?;
    delete_mode_for_parent(home, &parent)
}

fn delete_mode_for_parent(home: &Path, parent: &Path) -> io::Result<DeleteMode> {
    Ok(if trash::destinations(home, parent)?.is_empty() {
        DeleteMode::Permanent
    } else {
        DeleteMode::Trash
    })
}

/// 按已确认的 `mode` 删除 `path`，`home` 指定当前用户的回收站。
/// 回收站信息按 Trash 1.0 写入，已在回收站内的条目永久删除。
/// 原生名称以 URL 字节编码记录，先独占创建信息文件，再原子移动且禁止覆盖。
/// I/O 或跨卷失败保留源条目并删除本次信息文件；不会覆盖其他进程的同名条目。
///
/// # Errors
/// 文件访问、删除或回收站提交失败时返回 I/O 错误；不会在移动失败后改为永久删除。
pub fn delete(home: &Path, path: &Path, mode: DeleteMode) -> io::Result<Option<PathBuf>> {
    if mode == DeleteMode::Permanent {
        return purge(home, path).map(|()| None);
    }
    if in_trash(home, path) {
        return Err(io::Error::other("项目已位于回收站，请重新选择。"));
    }
    reject_mounted_subtree(path)?;
    let parent = fs::canonicalize(path.parent().unwrap_or_else(|| Path::new("/")))?;
    let original = parent.join(file_name(path)?);
    let mut unavailable = io::Error::other("无法移入此磁盘的回收站。");
    for volume in trash::destinations(home, &parent)? {
        match volume.prepare() {
            Ok(()) => return move_to_trash(&volume, &original).map(Some),
            Err(error) => unavailable = error,
        }
    }
    Err(unavailable)
}

fn move_to_trash(volume: &Volume, original: &Path) -> io::Result<PathBuf> {
    let files = volume.files();
    let recorded = match volume.top() {
        Some(top) => original.strip_prefix(top).map_err(io::Error::other)?,
        None => original,
    };
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    let seconds = i64::try_from(seconds).map_err(|_| io::Error::other("系统时间超出日期范围"))?;
    let local = calendar::zone::to_local(seconds);
    let date = calendar::civil_from_days(local.div_euclid(86_400));
    let day = local.rem_euclid(86_400);
    let contents = format!(
        "[Trash Info]\nPath={}\nDeletionDate={:04}-{:02}-{:02}T{:02}:{:02}:{:02}\n",
        encode_path(recorded),
        date.year,
        date.month,
        date.day,
        day / 3600,
        day / 60 % 60,
        day % 60
    );
    let mut basename = file_name(original)?;
    let short_name = OsStr::new("item");
    for number in 1..=u64::MAX {
        let target = files.join(numbered(basename, number));
        let info = info_path(volume, &target)?;
        let mut output = match absent(&target).and_then(|available| {
            available
                .then(|| OpenOptions::new().write(true).create_new(true).open(&info))
                .transpose()
        }) {
            Ok(Some(output)) => output,
            Ok(None) => continue,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            // 存储名还要容纳 .trashinfo 后缀；原名和原路径已完整记录在 Path。
            Err(error)
                if error.kind() == io::ErrorKind::InvalidFilename && basename != short_name =>
            {
                basename = short_name;
                continue;
            }
            Err(error) => return Err(error),
        };
        let result = output
            .write_all(contents.as_bytes())
            .and_then(|()| output.sync_all())
            .and_then(|()| rename_noreplace(original, &target));
        drop(output);
        match result {
            Ok(()) => return Ok(target),
            Err(error) => {
                fs::remove_file(&info)?;
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
        }
    }
    Err(io::Error::other("回收站文件名序号耗尽"))
}
fn erase(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn reject_mounted_subtree(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        let directory = fs::canonicalize(path)?;
        // remove_dir_all 会跨入挂载的子目录，包括同一文件系统的 bind mount。
        let mounts = fs::read("/proc/self/mounts")?;
        if mounts.split(|byte| *byte == b'\n').any(|line| {
            line.split(|byte| *byte == b' ')
                .nth(1)
                .is_some_and(|field| mount_path(field).starts_with(&directory))
        }) {
            return Err(io::Error::other("无法永久删除挂载点或包含挂载点的文件夹。"));
        }
    }
    Ok(())
}

fn visit_trash<'a>(
    volumes: &'a [Volume],
    mut visit: impl FnMut(&'a Volume, fs::DirEntry) -> io::Result<()>,
) -> io::Result<()> {
    let mut directories = BTreeSet::new();
    for volume in volumes {
        let directory = match fs::canonicalize(volume.files()) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata = fs::metadata(&directory)?;
        // 同一个回收目录可以经绑定挂载重复可见，只枚举一次。
        if !directories.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            visit(volume, entry?)?;
        }
    }
    Ok(())
}

fn containing_trash<'a>(volumes: &'a [Volume], path: &Path) -> Option<&'a Volume> {
    let parent = fs::canonicalize(path.parent()?).ok()?;
    volumes
        .iter()
        .find(|volume| fs::canonicalize(volume.files()).is_ok_and(|files| files == parent))
}

fn trash_record(home: &Path, path: &Path) -> io::Result<Option<PathBuf>> {
    containing_trash(&trash::volumes(home), path)
        .map(|volume| info_path(volume, path))
        .transpose()
}

fn remove_trash_record(info: Option<PathBuf>) -> io::Result<()> {
    if let Some(info) = info {
        match fs::remove_file(info) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

fn purge(home: &Path, path: &Path) -> io::Result<()> {
    reject_mounted_subtree(path)?;
    let info = trash_record(home, path)?;
    erase(path)?;
    remove_trash_record(info)
}
/// 将 `selected` 指定的回收站条目还原到原位置，`None` 表示当前用户的全部回收站。
/// 一同还原的父目录先于其子项，同名冲突时为还原条目选择新名称。
/// # Errors
/// 清单或元数据无法读取时不移动任何条目。执行阶段遇到错误停止，保留已完成的还原。
pub fn restore(home: &Path, selected: Option<Vec<PathBuf>>) -> io::Result<()> {
    let volumes = trash::volumes(home);
    let mut entries = Vec::new();
    let mut prepare = |volume, path: PathBuf| {
        let original = trash_info_of(volume, &path)?.original;
        let metadata = fs::symlink_metadata(&path)?;
        entries.push((
            volume,
            UndoItem {
                previous: original,
                current: path,
                identity: (metadata.dev(), metadata.ino()),
            },
        ));
        Ok(())
    };
    if let Some(paths) = selected {
        for path in paths {
            let volume = containing_trash(&volumes, &path).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "只能还原回收站根条目")
            })?;
            prepare(volume, path)?;
        }
    } else {
        visit_trash(&volumes, |volume, entry| prepare(volume, entry.path()))?;
    }
    // Path 按组件排序，共同前缀下父目录先于后代；与界面列排序无关。
    entries.sort_by(|(_, left), (_, right)| left.previous.cmp(&right.previous));
    for (volume, item) in entries {
        if !item.matches(&item.current) {
            return Err(io::Error::other("要还原的项目已移动、替换或不可访问。"));
        }
        restore_to(volume, &item.current, item.previous, false)?;
    }
    Ok(())
}

fn restore_undo(home: &Path, path: &Path, expected: &Path) -> io::Result<PathBuf> {
    let volumes = trash::volumes(home);
    let volume = containing_trash(&volumes, path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "只能还原回收站根条目"))?;
    let original = trash_info_of(volume, path)?.original;
    if expected != original {
        return Err(io::Error::other("回收站记录的原位置已更改。"));
    }
    restore_to(volume, path, original, true)
}

fn restore_to(volume: &Volume, path: &Path, original: PathBuf, exact: bool) -> io::Result<PathBuf> {
    let parent = original.parent().ok_or(io::ErrorKind::InvalidData)?;
    let mut ancestor = parent;
    let mut missing = Vec::new();
    let existing = loop {
        match fs::canonicalize(ancestor) {
            Ok(path) => break path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(ancestor);
                ancestor = ancestor.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    };
    if let Some(top) = volume.top()
        && (!existing.starts_with(top)
            || fs::metadata(&existing)?.dev() != fs::metadata(top)?.dev())
    {
        return Err(io::Error::other("原位置已移出此磁盘。"));
    }
    // The mounted directory may be writable while its parent is protected.
    // Invoke mkdir only for missing components, never for the existing mount point.
    for directory in missing.into_iter().rev() {
        match fs::create_dir(directory) {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && fs::symlink_metadata(directory).is_ok_and(|entry| entry.is_dir()) => {}
            Err(error) => return Err(error),
        }
    }
    let target = if exact {
        rename_noreplace(path, &original)?;
        original
    } else {
        move_unique(path, parent, file_name(&original)?)?
    };
    remove_trash_record(Some(info_path(volume, path)?))?;
    Ok(target)
}

fn preserve_modified(target: &File, source: &fs::Metadata) -> io::Result<()> {
    match target.set_modified(source.modified()?) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(()),
        result => result,
    }
}

fn copy_tree(from: &Path, to: &Path, moving: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
    }
    let result = if metadata.is_dir() {
        fs::create_dir(to)?;
        (|| {
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                copy_tree(&entry.path(), &to.join(entry.file_name()), moving)?;
            }
            // 普通复制产生新目录。跨卷移动要在子项写完后恢复目录时间，否则后续创建会覆盖它。
            if moving {
                preserve_modified(&File::open(to)?, &metadata)?;
            }
            fs::set_permissions(to, metadata.permissions())
        })()
    } else if metadata.is_file() {
        let mut input = File::open(from)?;
        let mut output = OpenOptions::new().write(true).create_new(true).open(to)?;
        io::copy(&mut input, &mut output)
            .and_then(|_| preserve_modified(&output, &metadata))
            .and_then(|_| output.set_permissions(metadata.permissions()))
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "只能复制普通文件、目录和符号链接",
        ));
    };
    // 只在原子创建成功后进入这里；创建失败直接返回，绝不清理竞争进程已有的条目。
    if result.is_err() {
        let _ = erase(to);
    }
    result
}
fn destination(from: &Path, into: &Path) -> io::Result<PathBuf> {
    let into = fs::canonicalize(into)?;
    if fs::symlink_metadata(from)?.is_dir() && into.starts_with(fs::canonicalize(from)?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "不能把文件夹复制或移动到它自己里面",
        ));
    }
    Ok(into)
}
/// 复制到目录，重名时使用“的副本”名称；拒绝规范化后位于源目录内的目标。
/// 原子创建避免覆盖；失败只清理本次创建的部分，不删除其他进程的条目。
/// 普通文件保留修改时间；目标不支持设置时间时仍复制内容。
pub fn copy(from: &Path, into: &Path) -> io::Result<PathBuf> {
    let directory = destination(from, into)?;
    copy_into(from, &directory, file_name(from)?, false)
}

fn copy_into(from: &Path, directory: &Path, name: &OsStr, moving: bool) -> io::Result<PathBuf> {
    let direct = directory.join(name);
    let suffix = if absent(&direct)? { "" } else { " 的副本" };
    loop {
        let target = free_name(directory, name, suffix)?;
        match copy_tree(from, &target, moving) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}
fn move_unique(from: &Path, into: &Path, name: &OsStr) -> io::Result<PathBuf> {
    loop {
        let target = free_name(into, name, "")?;
        match rename_noreplace(from, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}
/// 将 `from` 移动到目录 `into` 并返回目标。来自 `home` 回收站的根条目以记录的原名移出，完成后删除记录。
/// 规范化目录防止递归，原子移动禁止覆盖。
/// # Errors
/// 回收站记录无效时保留条目。跨文件系统时先复制再删除源；删除源失败保留已复制目标并报告错误。
pub fn relocate(home: &Path, from: &Path, into: &Path) -> io::Result<PathBuf> {
    let directory = destination(from, into)?;
    if from
        .parent()
        .is_some_and(|parent| fs::canonicalize(parent).is_ok_and(|parent| parent == directory))
    {
        return Ok(from.to_owned());
    }
    let volumes = trash::volumes(home);
    let volume = containing_trash(&volumes, from);
    let info = volume.map(|volume| info_path(volume, from)).transpose()?;
    let original = volume
        .map(|volume| trash_info_of(volume, from))
        .transpose()?;
    let name = file_name(original.as_ref().map_or(from, |entry| &entry.original))?;
    let target = match move_unique(from, &directory, name) {
        Ok(target) => Ok(target),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            reject_mounted_subtree(from)?;
            let target = copy_into(from, &directory, name, true)?;
            erase(from)?;
            Ok(target)
        }
        Err(error) => Err(error),
    }?;
    remove_trash_record(info)?;
    Ok(target)
}

/// 一批复制或移动的结果，供界面发布已完成部分并保留可重试的剪切项。
pub struct Transfer {
    pub result: io::Result<()>,
    pub remaining: Vec<PathBuf>,
    pub last: Option<PathBuf>,
}

/// 依次将 `paths` 复制或移动到 `into`。移动时使用 `home` 识别回收站。
/// 第一处错误停止处理，结果包含错误、从失败项开始的剩余路径与最后一个成功目标。
pub fn transfer(home: &Path, mut paths: Vec<PathBuf>, into: &Path, cut: bool) -> Transfer {
    let mut last = None;
    for (index, path) in paths.iter().enumerate() {
        let result = if cut {
            relocate(home, path, into)
        } else {
            copy(path, into)
        };
        match result {
            Ok(target) => last = Some(target),
            Err(error) => {
                paths.drain(..index);
                return Transfer {
                    result: Err(error),
                    remaining: paths,
                    last,
                };
            }
        }
    }
    Transfer {
        result: Ok(()),
        remaining: Vec::new(),
        last,
    }
}
/// 在目录里原子创建“新建文件夹”，同名时增加序号；I/O 失败返回错误。
/// 在 `dir` 中独占创建空文件 `name`（重名时按“name (2)”编号），对应外壳“新建 → 文本文档”。
pub fn new_file(dir: &Path, name: &str) -> io::Result<PathBuf> {
    loop {
        let target = free_name(dir, OsStr::new(name), "")?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(_) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}
pub fn new_folder(dir: &Path) -> io::Result<PathBuf> {
    loop {
        let target = free_name(dir, OsStr::new("新建文件夹"), "")?;
        match fs::create_dir(&target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("file-explorer-ops-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        fs::canonicalize(root).unwrap()
    }

    #[test]
    fn deleting_moves_to_trash_and_restoring_puts_it_back() {
        let root = scratch("trash");
        let home = root.join("home");
        let file = home.join("a.txt");
        fs::write(&file, "x").unwrap();
        delete(&home, &file, delete_mode(&home, &file).unwrap()).unwrap();
        assert!(!file.exists());
        let volume = Volume::home(&home);
        let trashed = volume.files().join("a.txt");
        assert_eq!(fs::read_to_string(&trashed).unwrap(), "x");
        // 源卷允许的完整文件名再加 .trashinfo 会超过目录项限制。
        let long = home.join(format!("{}字ab.txt", "a".repeat(246)));
        fs::write(&long, "long name").unwrap();
        let stored = move_to_trash(&volume, &long).unwrap();
        assert_eq!(trash_info_of(&volume, &stored).unwrap().original, long);
        assert!(!long.exists());
        restore(&home, Some(vec![stored])).unwrap();
        assert_eq!(fs::read_to_string(&long).unwrap(), "long name");
        let stored = move_to_trash(&volume, &long).unwrap();
        let record = info_path(&volume, &stored).unwrap();
        fs::write(&long, "existing name").unwrap();
        restore(&home, Some(vec![stored])).unwrap();
        let restored = home.join(format!("{} (2).txt", "a".repeat(246)));
        assert_eq!(fs::read_to_string(restored).unwrap(), "long name");
        assert_eq!(fs::read_to_string(&long).unwrap(), "existing name");
        assert!(!record.exists());
        let folder = home.join("folder");
        let child = folder.join("0-child");
        fs::create_dir(&folder).unwrap();
        fs::write(&child, "child").unwrap();
        fs::write(folder.join("marker"), "parent").unwrap();
        let trashed_child = move_to_trash(&volume, &child).unwrap();
        let trashed_folder = move_to_trash(&volume, &folder).unwrap();
        // 子项可在界面上排在父目录前；还原必须先将真正的父目录放回原处。
        restore(&home, Some(vec![trashed_child, trashed_folder])).unwrap();
        assert_eq!(fs::read_to_string(child).unwrap(), "child");
        assert_eq!(fs::read_to_string(folder.join("marker")).unwrap(), "parent");
        assert!(
            trashed.exists(),
            "selected restore must leave other entries"
        );
        // 原位置被占用时还原不覆盖，改用带序号的名字。
        fs::write(&file, "new").unwrap();
        restore(&home, None).unwrap();
        assert_eq!(fs::read_to_string(home.join("a (2).txt")).unwrap(), "x");
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert!(!volume.info().join("a.txt.trashinfo").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn undo_restores_original_names_and_preserves_conflicting_files() {
        let root = scratch("undo");
        let home = root.join("home");
        let original = home.join("first");
        fs::write(&original, "original").unwrap();
        let draft = Rename::prepare(original.clone()).unwrap();
        let backup = home.join("backup");
        fs::rename(&original, &backup).unwrap();
        fs::write(&original, "replacement").unwrap();
        assert_eq!(
            Undo::rename(&draft, "renamed").err().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs::read_to_string(&original).unwrap(), "replacement");
        assert!(!home.join("renamed").exists());
        fs::rename(&backup, &original).unwrap();
        let (renamed, mut rename) = Undo::rename(&draft, "renamed").unwrap();
        let other = home.join("other");
        fs::write(&other, "other").unwrap();
        let entries = Deletion::prepare(
            &home,
            vec![renamed.clone(), other.clone()],
            DeleteIntent::RecycleOnly,
        )
        .unwrap();
        let mut items = Vec::new();
        Deletion::commit(&home, entries, |item| items.push(item.unwrap())).unwrap();
        let mut delete = Undo::Delete(items);

        fs::write(&renamed, "replacement").unwrap();
        assert_eq!(
            delete.reverse(&home).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "replacement");
        assert!(!other.exists());

        fs::remove_file(&renamed).unwrap();
        delete.reverse(&home).unwrap();
        rename.reverse(&home).unwrap();
        assert_eq!(fs::read_to_string(&original).unwrap(), "original");
        assert_eq!(fs::read_to_string(&other).unwrap(), "other");
        assert!(!renamed.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleting_from_the_trash_is_permanent_and_same_names_do_not_collide() {
        let root = scratch("purge");
        let home = root.join("home");
        for _ in 0..2 {
            fs::write(home.join("same"), "x").unwrap();
            delete(&home, &home.join("same"), DeleteMode::Trash).unwrap();
        }
        assert!(Volume::home(&home).files().join("same (2)").exists());
        let duplicate = Volume::home(&home).files().join("same (2)");
        let confirmation =
            Deletion::prepare(&home, vec![duplicate], DeleteIntent::Permanent).unwrap();
        assert_eq!(confirmation[0].name(), "same");
        let trashed = Volume::home(&home).files().join("same");
        delete(&home, &trashed, delete_mode(&home, &trashed).unwrap()).unwrap();
        assert!(!Volume::home(&home).files().join("same").exists());
        assert!(!Volume::home(&home).info().join("same.trashinfo").exists());
        let entries = Deletion::prepare_empty_trash(&home).unwrap();
        assert_eq!(entries.len(), 1);
        let late = home.join("late");
        fs::write(&late, "added while confirming").unwrap();
        delete(&home, &late, DeleteMode::Trash).unwrap();
        Deletion::commit(&home, entries, |_| {}).unwrap();
        let volume = Volume::home(&home);
        assert!(!volume.files().join("same (2)").exists());
        assert!(!volume.info().join("same (2).trashinfo").exists());
        assert_eq!(
            fs::read_to_string(volume.files().join("late")).unwrap(),
            "added while confirming"
        );
        assert!(volume.info().join("late.trashinfo").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copying_names_duplicates_and_refuses_recursion() {
        let root = scratch("copy");
        let dir = root.join("home/d");
        fs::create_dir_all(dir.join("sub")).unwrap();
        let original = dir.join("sub/f.txt");
        fs::write(&original, "x").unwrap();
        let old = UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        File::open(&original).unwrap().set_modified(old).unwrap();
        File::open(&dir).unwrap().set_modified(old).unwrap();
        let copied = copy(&dir, &root.join("home")).unwrap();
        assert_eq!(copied, root.join("home/d 的副本"));
        assert_eq!(fs::read_to_string(copied.join("sub/f.txt")).unwrap(), "x");
        assert_eq!(
            fs::metadata(copied.join("sub/f.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            old
        );
        assert_ne!(fs::metadata(copied).unwrap().modified().unwrap(), old);
        assert!(copy(&dir, &dir.join("sub")).is_err());
        let file = root.join("home/f.txt");
        fs::write(&file, "y").unwrap();
        assert_eq!(
            copy(&file, &root.join("home")).unwrap(),
            root.join("home/f 的副本.txt")
        );
        let long = root.join("home").join(format!("{}.txt", "a".repeat(251)));
        fs::write(&long, "copy me").unwrap();
        let duplicate = copy(&long, &root.join("home")).unwrap();
        assert_eq!(
            duplicate.file_name().unwrap(),
            format!("{} 的副本.txt", "a".repeat(241)).as_str()
        );
        assert_eq!(fs::read_to_string(duplicate).unwrap(), "copy me");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn relocating_renaming_and_new_folders_validate_names() {
        let root = scratch("move");
        let home = root.join("home");
        fs::create_dir(home.join("into")).unwrap();
        fs::write(home.join("f"), "x").unwrap();
        let moved = relocate(&home, &home.join("f"), &home.join("into")).unwrap();
        assert_eq!(moved, home.join("into/f"));
        assert!(relocate(&home, &home.join("into"), &home.join("into")).is_err());
        for name in ["", "a/b", "项目:归档", "nul.txt", "lPt²", "名称.", "名称 "] {
            assert_eq!(
                rename(&moved, name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{name:?}"
            );
        }
        fs::write(home.join("into/g"), "").unwrap();
        assert_eq!(
            rename(&moved, "g").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            rename(&moved, "项目 归档").unwrap(),
            home.join("into/项目 归档")
        );
        let volume = Volume::home(&home);
        let repeated = home.join("original.txt");
        fs::write(&repeated, "first").unwrap();
        delete(&home, &repeated, DeleteMode::Trash).unwrap();
        fs::write(&repeated, "second").unwrap();
        let trashed = delete(&home, &repeated, DeleteMode::Trash)
            .unwrap()
            .unwrap();
        assert_eq!(trashed.file_name().unwrap(), "original (2).txt");
        let pasted = relocate(&home, &trashed, &home.join("into")).unwrap();
        assert_eq!(pasted, home.join("into/original.txt"));
        assert_eq!(fs::read_to_string(pasted).unwrap(), "second");
        assert!(!volume.info().join("original (2).txt.trashinfo").exists());
        assert!(volume.files().join("original.txt").exists());
        assert_eq!(new_folder(&home).unwrap(), home.join("新建文件夹"));
        assert_eq!(new_folder(&home).unwrap(), home.join("新建文件夹 (2)"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extension_splitting_keeps_dotfiles_whole() {
        let named = |name: &str| suffixed(OsStr::new(name), " (2)", usize::MAX);
        assert_eq!(named("a.tar.gz"), "a.tar (2).gz");
        assert_eq!(named(".profile"), ".profile (2)");
        assert_eq!(named("noext"), "noext (2)");
    }
}
