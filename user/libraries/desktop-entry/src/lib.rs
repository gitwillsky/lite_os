//! freedesktop Desktop Entry 子集：读取已安装程序的名称、图标、分类与命令；不把 Exec 当成 shell 程序解释。
extern crate liteos_runtime as _;
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

/// 一个可启动的程序条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// `Icon=` 的主题名称或绝对图像路径，没有时为 `None`。
    pub icon: Option<String>,
    /// `Comment=`（`Comment[zh_CN]` 优先）：控制面板“备注”列与 Web 视图说明。
    pub comment: Option<String>,
    /// `Path=` 的绝对工作目录；启动时由同一条目应用，不能丢失安装器的相对文件语义。
    working_directory: Option<PathBuf>,
    categories: Vec<String>,
    arguments: Vec<String>,
    /// `MimeType=` 声明可打开的类型。
    mime_types: Vec<String>,
    /// `StartupWMClass=`：该程序窗口的 `app_id`，任务栏、Alt+Tab 据此找到它的图标。
    wm_class: Option<String>,
    /// `NoDisplay=true`：不出现在程序列表里，但仍是已安装的程序（窗口图标查找要用）。
    no_display: bool,
    /// `Exec=` 带 `%f`/`%F`/`%u`/`%U`：启动时把要打开的文件追加为最后一个参数。
    takes_file: bool,
}

impl Entry {
    /// 返回尚未启动的命令，应用 Exec 参数与 Path 工作目录；参数不经过 shell。
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.arguments[0]);
        command.args(&self.arguments[1..]);
        if let Some(directory) = &self.working_directory {
            command.current_dir(directory);
        }
        command
    }

    /// 命令行参数（第一个是程序）；已按 Desktop Entry 规则分词，没有经过 shell。
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// 用这个程序打开 `file` 的命令：声明了文件字段码的程序把文件追加为最后一个参数；
    /// 没有声明的程序不能打开文件，返回 `None`。
    pub fn command_for(&self, file: &Path) -> Option<Command> {
        if !self.takes_file {
            return None;
        }
        let mut command = self.command();
        command.arg(file);
        Some(command)
    }

    /// 程序窗口的 `app_id`（`StartupWMClass=`）。
    pub fn wm_class(&self) -> Option<&str> {
        self.wm_class.as_deref()
    }

    pub fn in_category(&self, category: &str) -> bool {
        self.categories.iter().any(|c| c == category)
    }
}

/// 文件的 MIME 类型：按扩展名查 shared-mime-info 的一个子集；不认识的返回 `None`。
pub fn mime_for(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "wav" => "audio/x-wav",
        "exe" => "application/x-ms-dos-executable",
        "msi" => "application/x-msi",
        "txt" | "log" | "ini" | "cfg" | "conf" | "md" | "toml" | "json" | "sh" | "c" | "h"
        | "rs" | "py" | "xml" | "csv" => "text/plain",
        _ => return None,
    })
}

/// 按 XDG Base Directory 顺序返回用户与系统数据目录。
///
/// 用户目录优先，忽略非绝对 XDG 路径；未设置时使用规范默认值。返回值不要求目录已经存在。
pub fn data_directories() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let user = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".local/share"))
        });
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    user.into_iter()
        .chain(std::env::split_paths(&system).filter(|path| path.is_absolute()))
        .collect()
}

/// 读取一个标准应用快捷方式。
///
/// `path` 指定 `.desktop` 文件；返回已解析的名称、图标和无 shell 启动命令。
/// 文件不可读返回原始 I/O 错误，无效或隐藏条目返回 InvalidData。
pub fn read(path: &Path) -> io::Result<Entry> {
    parse(&fs::read_to_string(path)?)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid desktop entry"))
}

/// 为文件或应用快捷方式建立启动命令；不执行 shell。
///
/// `file` 指定绝对或相对文件路径；快捷方式优先于 Unix 可执行位，其次为原生可执行文件和已安装 MIME 处理器。
/// 返回尚未启动的 Command。文件不可读、无效快捷方式或没有关联程序时返回错误。
pub fn open(file: &Path) -> io::Result<Command> {
    if file
        .extension()
        .is_some_and(|extension| extension == "desktop")
    {
        return Ok(read(file)?.command());
    }
    if fs::metadata(file)?.permissions().mode() & 0o111 != 0
        && !file.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("msi")
        })
    {
        return Ok(Command::new(file));
    }
    let mime = mime_for(file).ok_or_else(|| io::Error::other("没有关联的程序"))?;
    installed_in(&data_directories())
        .iter()
        .find(|entry| entry.mime_types.iter().any(|value| value == mime))
        .and_then(|entry| entry.command_for(file))
        .ok_or_else(|| io::Error::other("没有关联的程序"))
}

/// 返回全部已安装且可显示的应用；用户同 ID 条目覆盖系统条目，Hidden 条目也遮蔽系统副本。
pub fn installed() -> Vec<Entry> {
    let mut entries = installed_in(&data_directories());
    entries.retain(|entry| !entry.no_display);
    entries
}

/// 返回窗口 app_id 与图标的对应关系，包含 NoDisplay 的已安装条目。
pub fn icons_by_app_id() -> HashMap<String, String> {
    installed_in(&data_directories())
        .into_iter()
        .filter_map(|entry| Some((entry.wm_class?, entry.icon?)))
        .collect()
}

fn desktop_files(directory: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                paths.extend(desktop_files(&entry.path()));
            } else if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "desktop")
            {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    paths
}

fn installed_in(directories: &[PathBuf]) -> Vec<Entry> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for directory in directories {
        let directory = directory.join("applications");
        for path in desktop_files(&directory) {
            let id = path
                .strip_prefix(&directory)
                .expect("descendant")
                .to_string_lossy()
                .replace('/', "-");
            // 先登记 ID 再解析：用户 Hidden 或无效条目不能使被覆盖的系统条目重新出现。
            if seen.insert(id)
                && let Ok(entry) = read(&path)
            {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn parse(text: &str) -> Option<Entry> {
    let mut in_entry = false;
    let mut name = None;
    let mut comment = None;
    let mut executable = None;
    let mut icon = None;
    let mut categories = Vec::new();
    let mut mime_types = Vec::new();
    let mut takes_file = false;
    let mut no_display = false;
    let mut wm_class = None;
    let mut application = false;
    let mut working_directory = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Name" if name.is_none() => name = Some(unescape(value)?),
            "Name[zh_CN]" => name = Some(unescape(value)?),
            "Comment" if comment.is_none() => comment = Some(unescape(value)?),
            "Comment[zh_CN]" => comment = Some(unescape(value)?),
            "Icon" => icon = Some(unescape(value)?),
            "Path" if !value.is_empty() => {
                let path = PathBuf::from(unescape(value)?);
                if !path.is_absolute() {
                    return None;
                }
                working_directory = Some(path);
            }
            "Categories" => {
                categories = value
                    .split(';')
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            "MimeType" => {
                mime_types = value
                    .split(';')
                    .filter(|m| !m.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            "Type" => application = value == "Application",
            "Exec" => {
                takes_file = value
                    .split_whitespace()
                    .any(|word| matches!(word, "%f" | "%F" | "%u" | "%U"));
                executable = Some(arguments(&unescape(value)?)?)
            }
            "NoDisplay" => no_display = value == "true",
            "StartupWMClass" => wm_class = Some(value.to_owned()),
            "Hidden" | "Terminal" if value == "true" => return None,
            _ => {}
        }
    }
    if !application {
        return None;
    }
    let arguments = executable?;
    if arguments.is_empty() {
        return None;
    }
    Some(Entry {
        name: name?,
        icon,
        comment,
        working_directory,
        categories,
        arguments,
        mime_types,
        takes_file,
        wm_class,
        no_display,
    })
}
/// Desktop Entry string 层先解码，再由 Exec 层处理双引号与反斜杠；Wine 的 Windows 路径需要这两层。
fn unescape(value: &str) -> Option<String> {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        result.push(if character == '\\' {
            match chars.next()? {
                's' => ' ',
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '\\' => '\\',
                _ => return None,
            }
        } else {
            character
        });
    }
    Some(result)
}

fn arguments(value: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut escape = false;
    for ch in value.chars() {
        if escape {
            word.push(ch);
            escape = false;
            continue;
        }
        match ch {
            '\\' => escape = true,
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if escape || quoted {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    let mut result = Vec::new();
    for word in words {
        if matches!(word.as_str(), "%f" | "%F" | "%u" | "%U") {
            continue;
        }
        let word = word.replace("%%", "\0");
        if word.contains('%') {
            return None;
        }
        result.push(word.replace('\0', "%"));
    }
    Some(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exec_does_not_invoke_a_shell_or_expand_environment() {
        assert_eq!(
            arguments("/bin/app \"two words\" '$HOME' %% %f").unwrap(),
            ["/bin/app", "two words", "'$HOME'", "%"]
        );
        assert!(arguments("/bin/app \"broken").is_none());
        assert!(arguments("/bin/app %Z").is_none());
        assert!(
            parse("[Desktop Entry]\nType=Application\nName=X\nExec=/bin/x\nTerminal=true")
                .is_none()
        );
    }
    #[test]
    fn wine_entry_preserves_two_escape_layers_and_working_directory() {
        let entry = parse(r#"[Desktop Entry]
Type=Application
Name=Win32 Notepad
Exec=env "WINEPREFIX=/home/alice/.wine" wine "C:\\\\Program Files\\\\Demo\\\\notepad.exe" "--literal=$HOME"
Path=/home/alice/.wine/drive_c/Program Files/Demo
Icon=/home/alice/.local/share/icons/notepad.png
StartupWMClass=notepad.exe
"#).unwrap();
        assert_eq!(
            entry.arguments(),
            [
                "env",
                "WINEPREFIX=/home/alice/.wine",
                "wine",
                r"C:\Program Files\Demo\notepad.exe",
                "--literal=$HOME"
            ]
        );
        let command = entry.command();
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/home/alice/.wine/drive_c/Program Files/Demo"))
        );
        assert_eq!(entry.wm_class(), Some("notepad.exe"));
    }

    #[test]
    fn user_entries_override_system_and_hidden_masks_nested_wine_entry() {
        let root = std::env::temp_dir().join(format!("liteos-entry-{}", std::process::id()));
        let user = root.join("user");
        let system = root.join("system");
        fs::create_dir_all(user.join("applications/wine/Programs")).unwrap();
        fs::create_dir_all(system.join("applications/wine/Programs")).unwrap();
        let app = "[Desktop Entry]\nType=Application\nName=System\nExec=/bin/false\n";
        fs::write(system.join("applications/wine/Programs/demo.desktop"), app).unwrap();
        fs::write(
            user.join("applications/wine-Programs-demo.desktop"),
            format!("{app}Hidden=true\n"),
        )
        .unwrap();
        assert!(installed_in(&[user.clone(), system.clone()]).is_empty());
        fs::remove_file(user.join("applications/wine-Programs-demo.desktop")).unwrap();
        fs::write(
            user.join("applications/wine/Programs/demo.desktop"),
            app.replace("System", "User"),
        )
        .unwrap();
        let entries = installed_in(&[user, system]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "User");
        fs::remove_dir_all(root).unwrap();
    }
}
