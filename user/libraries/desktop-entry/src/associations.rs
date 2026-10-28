//! 文件类型与应用关联；XDG mimeapps.list 拥有用户默认值，应用条目拥有可接受的 MIME 与启动命令。
use super::{Entry, data_directories, installed_in, mime::Database};
use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Default)]
struct MimeApps {
    defaults: HashMap<String, Vec<String>>,
    added: HashMap<String, Vec<String>>,
    removed: HashMap<String, Vec<String>>,
    data_rank: Option<usize>,
    desktop_specific: bool,
}

impl MimeApps {
    fn read(path: &Path, data_rank: Option<usize>, desktop_specific: bool) -> Self {
        let mut result = Self {
            data_rank,
            desktop_specific,
            ..Self::default()
        };
        let mut section = "";
        let text = fs::read_to_string(path).unwrap_or_default();
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line;
                continue;
            }
            let Some((mime, value)) = line.split_once('=') else {
                continue;
            };
            let target = match section {
                "[Default Applications]" => &mut result.defaults,
                "[Added Associations]" if !desktop_specific => &mut result.added,
                "[Removed Associations]" if !desktop_specific => &mut result.removed,
                _ => continue,
            };
            target.insert(
                mime.trim().to_owned(),
                value
                    .split(';')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .collect(),
            );
        }
        result
    }
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into())).join(".config")
        })
}

/// 一次读取的安装条目和关联配置；目录批量描述共用此快照，避免逐文件重读应用目录。
pub struct Associations {
    entries: Vec<Entry>,
    preferences: Vec<MimeApps>,
    database: Database,
}

impl Associations {
    /// 按 XDG config/data 优先级读取当前用户的应用与 mimeapps.list；缺失配置视为空配置。
    pub fn load() -> Self {
        let data = data_directories();
        let mut directories = vec![(config_home(), None)];
        let config = std::env::var_os("XDG_CONFIG_DIRS")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/etc/xdg".into());
        directories.extend(
            std::env::split_paths(&config)
                .filter(|p| p.is_absolute())
                .map(|p| (p, None)),
        );
        directories.extend(
            data.iter()
                .enumerate()
                .map(|(rank, p)| (p.join("applications"), Some(rank))),
        );
        let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .filter(|s| !s.is_empty() && !s.contains('/'))
            .map(str::to_ascii_lowercase)
            .collect();
        let mut preferences = Vec::new();
        for (directory, rank) in directories {
            for desktop in &desktops {
                preferences.push(MimeApps::read(
                    &directory.join(format!("{desktop}-mimeapps.list")),
                    rank,
                    true,
                ));
            }
            preferences.push(MimeApps::read(
                &directory.join("mimeapps.list"),
                rank,
                false,
            ));
        }
        Self {
            entries: installed_in(&data),
            preferences,
            database: Database::load(&data),
        }
    }

    fn supports(&self, entry: &Entry, mime: &str) -> bool {
        if !entry.takes_file {
            return false;
        }
        for preference in &self.preferences {
            if preference
                .data_rank
                .is_some_and(|rank| rank > entry.data_rank)
            {
                continue;
            }
            if preference
                .added
                .get(mime)
                .is_some_and(|ids| ids.contains(&entry.id))
            {
                return true;
            }
            if preference
                .removed
                .get(mime)
                .is_some_and(|ids| ids.contains(&entry.id))
            {
                return false;
            }
        }
        entry
            .mime_types
            .iter()
            .any(|value| self.database.canonical(value) == mime)
    }

    /// 返回可接收文件的已安装应用，关联该类型的应用在前；其余应用可用于临时“打开方式”。
    pub fn choices(&self, path: &Path) -> Vec<Entry> {
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| entry.takes_file)
            .cloned()
            .collect();
        let types = self
            .mime_for(path)
            .map(|mime| self.database.hierarchy(mime))
            .unwrap_or_default();
        entries.sort_by_key(|entry| !types.iter().any(|mime| self.supports(entry, mime)));
        entries
    }

    /// 返回按标准默认值、Added/Removed Associations 和应用声明选出的处理器；未知类型返回 None。
    pub fn default_for(&self, path: &Path) -> Option<&Entry> {
        self.database
            .hierarchy(self.mime_for(path)?)
            .into_iter()
            .find_map(|mime| self.default_mime(mime))
    }

    fn default_mime(&self, mime: &str) -> Option<&Entry> {
        for preference in &self.preferences {
            for id in preference.defaults.get(mime).into_iter().flatten() {
                if let Some(entry) = self
                    .entries
                    .iter()
                    .find(|entry| entry.id == *id && self.supports(entry, mime))
                {
                    return Some(entry);
                }
            }
        }
        for preference in &self.preferences {
            for id in preference.added.get(mime).into_iter().flatten() {
                if let Some(entry) = self.entries.iter().find(|entry| {
                    entry.id == *id
                        && preference
                            .data_rank
                            .is_none_or(|rank| rank <= entry.data_rank)
                        && self.supports(entry, mime)
                }) {
                    return Some(entry);
                }
            }
            if !preference.desktop_specific
                && let Some(rank) = preference.data_rank
                && let Some(entry) = self
                    .entries
                    .iter()
                    .find(|entry| entry.data_rank == rank && self.supports(entry, mime))
            {
                return Some(entry);
            }
        }
        None
    }

    /// 按 Shared MIME-info 注册的 glob 查询文件名；未知格式返回 None，不读取文件内容。
    pub fn mime_for(&self, path: &Path) -> Option<&str> {
        self.database.mime_for(path)
    }

    /// 返回注册的本地化类型名称；未知类型显示扩展名。
    pub fn type_name(&self, path: &Path) -> String {
        if let Some(record) = self
            .mime_for(path)
            .and_then(|mime| self.database.record(mime))
            && !record.name.is_empty()
        {
            return record.name.clone();
        }
        path.extension().map_or_else(
            || "文件".to_owned(),
            |extension| format!("{} 文件", extension.to_string_lossy().to_uppercase()),
        )
    }

    /// 返回 MIME 的文件图标；未声明时沿用默认应用图标。名称交由标准主题图标加载器解析。
    pub fn icon_for(&self, path: &Path) -> Option<&str> {
        self.mime_for(path)
            .and_then(|mime| self.database.record(mime))
            .and_then(|record| record.icon.as_deref())
            .or_else(|| {
                self.default_for(path)
                    .and_then(|entry| entry.icon.as_deref())
            })
    }

    /// 保存用户默认处理器及显式关联，移除同层相反的 Removed 条目；未知类型或已卸载应用返回 InvalidInput。
    /// 写入采用锁内读取/合并与原子发布；失败保留原文件并清除本次临时文件。
    pub fn set_default(&self, path: &Path, entry: &Entry) -> io::Result<()> {
        let mime = self.mime_for(path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "未知文件类型不能保存默认程序")
        })?;
        if !self
            .entries
            .iter()
            .any(|installed| installed.id == entry.id && installed.takes_file)
            || entry.id.contains([';', '=', '\n', '\r'])
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let directory = config_home();
        fs::create_dir_all(&directory)?;
        // OWNER: 此文件锁只序列化本用户关联配置的读/合并/发布，避免并行窗口覆盖其他类型的修改。
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(".mimeapps.list.lock"))?;
        lock.lock()?;
        let path = directory.join("mimeapps.list");
        let mut text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let current = MimeApps::read(&path, None, false);
        let mut added = vec![entry.id.clone()];
        added.extend(
            current
                .added
                .get(mime)
                .into_iter()
                .flatten()
                .filter(|id| **id != entry.id)
                .cloned(),
        );
        let removed: Vec<_> = current
            .removed
            .get(mime)
            .into_iter()
            .flatten()
            .filter(|id| **id != entry.id)
            .cloned()
            .collect();
        replace(
            &mut text,
            "Default Applications",
            mime,
            std::slice::from_ref(&entry.id),
        );
        replace(&mut text, "Added Associations", mime, &added);
        replace(&mut text, "Removed Associations", mime, &removed);
        let mut temporary = None;
        for index in 0..128 {
            let candidate = directory.join(format!(".mimeapps.{}.{index}.tmp", std::process::id()));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => {
                    temporary = Some((candidate, file));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let (temporary, mut file) = temporary.ok_or(io::ErrorKind::AlreadyExists)?;
        let result = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, &path));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn replace(text: &mut String, section: &str, mime: &str, ids: &[String]) {
    let header = format!("[{section}]");
    let value = format!(
        "{mime}={}\n",
        if ids.is_empty() {
            String::new()
        } else {
            format!("{};", ids.join(";"))
        }
    );
    let mut output = String::new();
    let mut inside = false;
    let mut written = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if inside && !written {
                output.push_str(&value);
                written = true;
            }
            inside = trimmed == header;
        }
        if inside
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == mime)
        {
            if !written {
                output.push_str(&value);
                written = true;
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if !written {
        if !inside {
            output.push('\n');
            output.push_str(&header);
            output.push('\n');
        }
        output.push_str(&value);
    }
    *text = output;
}
