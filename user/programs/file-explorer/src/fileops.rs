//! 文件操作：失败只清理本次创建的条目，命名冲突由原子创建/移动裁决。
use linux_uapi::filesystem::rename_noreplace;
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::DirBuilderExt,
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// 当前用户的 home trash；只支持同一文件系统，跨卷失败保留源条目。
pub fn trash_files(home: &Path) -> PathBuf {
    trash_root(home).join("files")
}
fn trash_root(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"))
        .join("Trash")
}
fn trash_info(home: &Path) -> PathBuf {
    trash_root(home).join("info")
}
fn info_path(home: &Path, path: &Path) -> io::Result<PathBuf> {
    let mut name = file_name(path)?.to_os_string();
    name.push(".trashinfo");
    Ok(trash_info(home).join(name))
}
fn file_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "路径没有可用的名称"))
}
fn numbered(name: &OsStr, number: u64) -> OsString {
    if number == 1 {
        return name.to_owned();
    }
    suffixed(name, &format!(" ({number})"))
}
fn suffixed(name: &OsStr, suffix: &str) -> OsString {
    let bytes = name.as_bytes();
    let split = bytes
        .iter()
        .rposition(|byte| *byte == b'.')
        .filter(|index| *index != 0)
        .unwrap_or(bytes.len());
    let mut name = OsString::from_vec(bytes[..split].to_vec());
    name.push(suffix);
    name.push(OsStr::from_bytes(&bytes[split..]));
    name
}
fn absent(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}
fn free_name(dir: &Path, name: &OsStr) -> io::Result<PathBuf> {
    for number in 1..=u64::MAX {
        let candidate = dir.join(numbered(name, number));
        if absent(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(io::Error::other("文件名序号耗尽"))
}

/// 路径是否位于 home trash；调用方传入资源管理器已规范化的所在目录。
pub fn in_trash(home: &Path, path: &Path) -> bool {
    let files = trash_files(home);
    path.starts_with(&files) || fs::canonicalize(&files).is_ok_and(|files| path.starts_with(files))
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
    let path = PathBuf::from(OsString::from_vec(decoded));
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "home trash 路径必须是绝对路径",
        ));
    }
    Ok(path)
}

/// 将条目移入 home trash；信息按 Trash 1.0 写入，已在回收站内的条目永久删除。
/// 原生名称以 URL 字节编码记录，先独占创建信息文件，再原子移动且禁止覆盖。
/// I/O 或跨卷失败保留源条目并删除本次信息文件；不会覆盖其他进程的同名条目。
pub fn delete(home: &Path, path: &Path) -> io::Result<()> {
    if in_trash(home, path) {
        return purge(home, path);
    }
    let files = trash_files(home);
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(&files)?;
    builder.create(trash_info(home))?;
    let original =
        fs::canonicalize(path.parent().unwrap_or(Path::new("/")))?.join(file_name(path)?);
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
        encode_path(&original),
        date.year,
        date.month,
        date.day,
        day / 3600,
        day / 60 % 60,
        day % 60
    );
    for number in 1..=u64::MAX {
        let target = files.join(numbered(file_name(path)?, number));
        if !absent(&target)? {
            continue;
        }
        let info = info_path(home, &target)?;
        let mut output = match OpenOptions::new().write(true).create_new(true).open(&info) {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = output
            .write_all(contents.as_bytes())
            .and_then(|()| output.sync_all())
            .and_then(|()| rename_noreplace(path, &target));
        drop(output);
        match result {
            Ok(()) => return Ok(()),
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
fn purge(home: &Path, path: &Path) -> io::Result<()> {
    // 内部子项删除不能删除根条目的 .trashinfo，也不能把同名内部文件当作根条目。
    let info = path
        .parent()
        .filter(|parent| {
            *parent == trash_files(home)
                || fs::canonicalize(trash_files(home)).is_ok_and(|files| *parent == files)
        })
        .map(|_| info_path(home, path))
        .transpose()?;
    erase(path)?;
    if let Some(info) = info {
        match fs::remove_file(info) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}
/// 将 home trash 根条目还原到原位置，冲突时选择新名称。
/// 信息缺失/编码损坏或移动失败返回错误；原子移动从不覆盖已有目标。
pub fn restore(home: &Path, path: &Path) -> io::Result<PathBuf> {
    let files = fs::canonicalize(trash_files(home))?;
    let parent = fs::canonicalize(path.parent().unwrap_or(Path::new("/")))?;
    if parent != files {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "只能还原回收站根条目",
        ));
    }
    let info = info_path(home, path)?;
    let text = fs::read_to_string(&info)?;
    if text.lines().next() != Some("[Trash Info]") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "回收站信息文件损坏",
        ));
    }
    let original = text
        .lines()
        .skip(1)
        .find_map(|line| line.strip_prefix("Path="))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "回收站缺少原路径"))?;
    let original = decode_path(original)?;
    let parent = original.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(parent)?;
    let target = move_unique(path, parent, file_name(&original)?)?;
    fs::remove_file(info)?;
    Ok(target)
}
/// 清空 home trash，返回已删除根条目数；任何 I/O 错误立即返回。
pub fn empty_trash(home: &Path) -> io::Result<usize> {
    let mut count = 0;
    match fs::read_dir(trash_files(home)) {
        Ok(entries) => {
            for entry in entries {
                purge(home, &entry?.path())?;
                count += 1;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(count)
}
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
    }
    let result = if metadata.is_dir() {
        fs::create_dir(to)?;
        (|| {
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                copy_tree(&entry.path(), &to.join(entry.file_name()))?;
            }
            fs::set_permissions(to, metadata.permissions())
        })()
    } else if metadata.is_file() {
        let mut input = File::open(from)?;
        let mut output = OpenOptions::new().write(true).create_new(true).open(to)?;
        io::copy(&mut input, &mut output)
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
pub fn copy(from: &Path, into: &Path) -> io::Result<PathBuf> {
    let directory = destination(from, into)?;
    let name = file_name(from)?;
    let direct = directory.join(name);
    let name = if absent(&direct)? {
        name.to_owned()
    } else {
        suffixed(name, " 的副本")
    };
    loop {
        let target = free_name(&directory, &name)?;
        match copy_tree(from, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}
fn move_unique(from: &Path, into: &Path, name: &OsStr) -> io::Result<PathBuf> {
    loop {
        let target = free_name(into, name)?;
        match rename_noreplace(from, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}
/// 移动到目录，规范化目录防止递归，原子移动禁止覆盖。
/// 跨文件系统时先复制再删除源；删除源失败保留已复制目标并报告错误。
pub fn relocate(from: &Path, into: &Path) -> io::Result<PathBuf> {
    let directory = destination(from, into)?;
    if from
        .parent()
        .is_some_and(|parent| fs::canonicalize(parent).is_ok_and(|parent| parent == directory))
    {
        return Ok(from.to_owned());
    }
    match move_unique(from, &directory, file_name(from)?) {
        Ok(target) => Ok(target),
        Err(error) if error.raw_os_error() == Some(18) => {
            let target = copy(from, &directory)?;
            erase(from)?;
            Ok(target)
        }
        Err(error) => Err(error),
    }
}
/// 在同目录改名；名称不能空、含 / 或 NUL，也不能是 . 或 ..。
/// 目标已存在返回 AlreadyExists，其他失败返回 I/O 错误；不覆盖竞争创建的文件。
pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    if new_name.is_empty() || new_name.contains(['/', '\0']) || matches!(new_name, "." | "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "文件名不能为空，也不能包含 / 或 NUL",
        ));
    }
    let target = path.with_file_name(new_name);
    if target == path {
        return Ok(target);
    }
    rename_noreplace(path, &target)?;
    Ok(target)
}
/// 在目录里原子创建“新建文件夹”，同名时增加序号；I/O 失败返回错误。
pub fn new_folder(dir: &Path) -> io::Result<PathBuf> {
    loop {
        let target = free_name(dir, OsStr::new("新建文件夹"))?;
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
        root
    }

    #[test]
    fn deleting_moves_to_trash_and_restoring_puts_it_back() {
        let root = scratch("trash");
        let home = root.join("home");
        let file = home.join("a.txt");
        fs::write(&file, "x").unwrap();
        delete(&home, &file).unwrap();
        assert!(!file.exists());
        let trashed = trash_files(&home).join("a.txt");
        assert_eq!(fs::read_to_string(&trashed).unwrap(), "x");
        // 原位置被占用时还原不覆盖，改用带序号的名字。
        fs::write(&file, "new").unwrap();
        let restored = restore(&home, &trashed).unwrap();
        // 信息文件记录规范化后的原路径，宿主机的临时目录可能是符号链接。
        assert_eq!(restored, fs::canonicalize(&home).unwrap().join("a (2).txt"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert!(!trash_info(&home).join("a.txt.trashinfo").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn deleting_from_the_trash_is_permanent_and_same_names_do_not_collide() {
        let root = scratch("purge");
        let home = root.join("home");
        for _ in 0..2 {
            fs::write(home.join("same"), "x").unwrap();
            delete(&home, &home.join("same")).unwrap();
        }
        assert!(trash_files(&home).join("same (2)").exists());
        delete(&home, &trash_files(&home).join("same")).unwrap();
        assert!(!trash_files(&home).join("same").exists());
        assert!(!trash_info(&home).join("same.trashinfo").exists());
        assert_eq!(empty_trash(&home).unwrap(), 1);
        assert_eq!(fs::read_dir(trash_files(&home)).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copying_names_duplicates_and_refuses_recursion() {
        let root = scratch("copy");
        let dir = root.join("home/d");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/f.txt"), "x").unwrap();
        let copied = copy(&dir, &root.join("home")).unwrap();
        assert_eq!(copied, root.join("home/d 的副本"));
        assert_eq!(fs::read_to_string(copied.join("sub/f.txt")).unwrap(), "x");
        assert!(copy(&dir, &dir.join("sub")).is_err());
        let file = root.join("home/f.txt");
        fs::write(&file, "y").unwrap();
        assert_eq!(
            copy(&file, &root.join("home")).unwrap(),
            root.join("home/f 的副本.txt")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn relocating_renaming_and_new_folders_validate_names() {
        let root = scratch("move");
        let home = root.join("home");
        fs::create_dir(home.join("into")).unwrap();
        fs::write(home.join("f"), "x").unwrap();
        let moved = relocate(&home.join("f"), &home.join("into")).unwrap();
        assert_eq!(moved, home.join("into/f"));
        assert!(relocate(&home.join("into"), &home.join("into")).is_err());
        assert!(rename(&moved, "").is_err());
        assert!(rename(&moved, "a/b").is_err());
        fs::write(home.join("into/g"), "").unwrap();
        assert_eq!(
            rename(&moved, "g").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(rename(&moved, "h").unwrap(), home.join("into/h"));
        assert_eq!(new_folder(&home).unwrap(), home.join("新建文件夹"));
        assert_eq!(new_folder(&home).unwrap(), home.join("新建文件夹 (2)"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extension_splitting_keeps_dotfiles_whole() {
        assert_eq!(split_extension("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_extension(".profile"), (".profile", ""));
        assert_eq!(split_extension("noext"), ("noext", ""));
    }
}
