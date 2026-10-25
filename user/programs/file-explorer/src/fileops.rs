//! 文件操作：回收站、复制、移动、重命名、新建文件夹。回收站遵循 freedesktop Trash 规范的 `files/` + `info/` 布局。
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// 回收站的 `files` 目录；`$HOME` 下 `.local/share/Trash/files`。
pub fn trash_files(home: &Path) -> PathBuf {
    home.join(".local/share/Trash/files")
}

fn trash_info(home: &Path) -> PathBuf {
    home.join(".local/share/Trash/info")
}

/// 在 `dir` 里找一个不冲突的名字：`name` 已存在时依次尝试 `name (2)`、`name (3)` ……
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if fs::symlink_metadata(&candidate).is_err() {
        return candidate;
    }
    let (stem, extension) = split_extension(name);
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){extension}")))
        .find(|path| fs::symlink_metadata(path).is_err())
        .expect("an unbounded counter always finds a free name")
}

/// 名称拆成主干与扩展名（含点）；隐藏文件 `.profile` 与无扩展名文件的扩展名为空。
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(dot) => name.split_at(dot),
    }
}

fn file_name(path: &Path) -> io::Result<&str> {
    path.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "路径没有可用的名称"))
}

/// 路径是否位于回收站内。
pub fn in_trash(home: &Path, path: &Path) -> bool {
    path.starts_with(trash_files(home))
}

/// 把条目移入回收站并写下原路径（用于还原）。回收站里的条目则永久删除。
///
/// # Errors
///
/// 重命名、写信息文件或删除失败；失败时不会留下半个信息文件。
pub fn delete(home: &Path, path: &Path) -> io::Result<()> {
    if in_trash(home, path) {
        return purge(home, path);
    }
    let files = trash_files(home);
    fs::create_dir_all(&files)?;
    fs::create_dir_all(trash_info(home))?;
    let target = free_name(&files, file_name(path)?);
    let info = trash_info(home).join(format!("{}.trashinfo", file_name(&target)?));
    let original =
        fs::canonicalize(path.parent().unwrap_or(Path::new("/")))?.join(file_name(path)?);
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    fs::write(
        &info,
        format!(
            "[Trash Info]\nPath={}\nDeletionDate={seconds}\n",
            original.display()
        ),
    )?;
    if let Err(error) = fs::rename(path, &target) {
        let _ = fs::remove_file(&info);
        return Err(error);
    }
    Ok(())
}

/// 永久删除回收站里的一个条目及其信息文件。
fn purge(home: &Path, path: &Path) -> io::Result<()> {
    let info = trash_info(home).join(format!("{}.trashinfo", file_name(path)?));
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    match fs::remove_file(info) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// 把回收站里的条目放回原位置；原位置已有同名项时改用不冲突的名字。
///
/// # Errors
///
/// 信息文件缺失/损坏，或移动失败。
pub fn restore(home: &Path, path: &Path) -> io::Result<PathBuf> {
    let info = trash_info(home).join(format!("{}.trashinfo", file_name(path)?));
    let text = fs::read_to_string(&info)?;
    let original = text
        .lines()
        .find_map(|line| line.strip_prefix("Path="))
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "回收站信息文件损坏"))?;
    let parent = original.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(parent)?;
    let target = free_name(parent, file_name(&original)?);
    fs::rename(path, &target)?;
    fs::remove_file(info)?;
    Ok(target)
}

/// 清空回收站，返回删除的条目数。
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
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if metadata.is_dir() {
        // 目标已先创建；把自己复制进自己会无限递归，必须拒绝。
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let child = to.join(entry.file_name());
            if child == from {
                continue;
            }
            copy_tree(&entry.path(), &child)?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(drop)
    }
}

/// 复制到目录 `into`，重名时改名为"X 的副本"；返回新路径。
///
/// # Errors
///
/// 目标位于源目录内部（会无限递归），或任何 I/O 失败；失败时已复制的部分被删除。
pub fn copy(from: &Path, into: &Path) -> io::Result<PathBuf> {
    if fs::symlink_metadata(from)?.is_dir() && into.starts_with(from) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "不能把文件夹复制到它自己里面",
        ));
    }
    let name = file_name(from)?;
    let direct = into.join(name);
    let target = if fs::symlink_metadata(&direct).is_err() {
        direct
    } else {
        let (stem, extension) = split_extension(name);
        free_name(into, &format!("{stem} 的副本{extension}"))
    };
    if let Err(error) = copy_tree(from, &target) {
        let _ = fs::remove_dir_all(&target).or_else(|_| fs::remove_file(&target));
        return Err(error);
    }
    Ok(target)
}

/// 移动到目录 `into`（剪切后粘贴）；同名时改名为不冲突的名字。
///
/// # Errors
///
/// 目标位于源目录内部，或重命名失败（跨文件系统时先复制再删除源）。
pub fn relocate(from: &Path, into: &Path) -> io::Result<PathBuf> {
    if fs::symlink_metadata(from)?.is_dir() && into.starts_with(from) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "不能把文件夹移动到它自己里面",
        ));
    }
    if from.parent() == Some(into) {
        return Ok(from.to_owned());
    }
    let target = free_name(into, file_name(from)?);
    match fs::rename(from, &target) {
        Ok(()) => Ok(target),
        Err(error) if error.raw_os_error() == Some(18) => {
            let copied = copy_tree(from, &target).and_then(|()| {
                if fs::symlink_metadata(from)?.is_dir() {
                    fs::remove_dir_all(from)
                } else {
                    fs::remove_file(from)
                }
            });
            copied.map(|()| target)
        }
        Err(error) => Err(error),
    }
}

/// 在同一目录里改名。名称不能为空、含 `/` 或 NUL，且不能覆盖已有项。
///
/// # Errors
///
/// 名称非法、目标已存在或重命名失败。
pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    if new_name.is_empty() || new_name.contains(['/', '\0']) || matches!(new_name, "." | "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "文件名不能为空，也不能包含 / 字符",
        ));
    }
    let target = path.with_file_name(new_name);
    if target == path {
        return Ok(target);
    }
    if fs::symlink_metadata(&target).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "已存在同名的文件或文件夹",
        ));
    }
    fs::rename(path, &target)?;
    Ok(target)
}

/// 在 `dir` 里新建"新建文件夹"（重名时加序号）。
pub fn new_folder(dir: &Path) -> io::Result<PathBuf> {
    let target = free_name(dir, "新建文件夹");
    fs::create_dir(&target)?;
    Ok(target)
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
