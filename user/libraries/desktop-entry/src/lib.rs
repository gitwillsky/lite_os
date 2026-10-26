//! freedesktop Desktop Entry 子集：读取已安装程序的名称、图标、分类与命令；不把 Exec 当成 shell 程序解释。
extern crate liteos_runtime as _;
use std::{fs, path::Path, process::Command};

/// 一个可启动的程序条目。
pub struct Entry {
    pub name: String,
    /// `Icon=` 的名称（不是路径），没有时为 `None`。
    pub icon: Option<String>,
    /// `Comment=`（`Comment[zh_CN]` 优先）：控制面板“备注”列与 Web 视图说明。
    pub comment: Option<String>,
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
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.arguments[0]);
        command.args(&self.arguments[1..]);
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
        "txt" | "log" | "ini" | "cfg" | "conf" | "md" | "toml" | "json" | "sh" | "c" | "h"
        | "rs" | "py" | "xml" | "csv" => "text/plain",
        _ => return None,
    })
}

/// 在 `directory` 的桌面条目里找出声明能打开 `file` 的第一个程序，返回打开它的命令。
pub fn open_with(directory: &Path, file: &Path) -> Option<Command> {
    let mime = mime_for(file)?;
    load(directory)
        .iter()
        .find(|entry| entry.mime_types.iter().any(|m| m == mime))
        .and_then(|entry| entry.command_for(file))
}

/// `app_id` 到 `Icon=` 名称的对照表，来自声明了 `StartupWMClass=` 与 `Icon=` 的条目（含 `NoDisplay` 的）。
pub fn icons_by_app_id(directory: &Path) -> std::collections::HashMap<String, String> {
    load_all(directory)
        .into_iter()
        .filter_map(|entry| Some((entry.wm_class?, entry.icon?)))
        .collect()
}

/// 读取目录里的程序列表条目（不含 `NoDisplay`），按名称排序；目录不存在或条目无效时跳过。
pub fn load(directory: &Path) -> Vec<Entry> {
    let mut entries = load_all(directory);
    entries.retain(|entry| !entry.no_display);
    entries
}

/// 读取目录里的全部有效 `.desktop` 条目，包括 `NoDisplay` 的，按名称排序。
pub fn load_all(directory: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    if let Ok(directory) = fs::read_dir(directory) {
        for entry in directory
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|s| s == "desktop"))
        {
            if let Ok(text) = fs::read_to_string(entry.path())
                && let Some(app) = parse(&text)
            {
                entries.push(app);
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
            "Name" if name.is_none() => name = Some(value.to_owned()),
            "Name[zh_CN]" => name = Some(value.to_owned()),
            "Comment" if comment.is_none() => comment = Some(value.to_owned()),
            "Comment[zh_CN]" => comment = Some(value.to_owned()),
            "Icon" => icon = Some(value.to_owned()),
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
                executable = Some(arguments(value)?)
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
        categories,
        arguments,
        mime_types,
        takes_file,
        wm_class,
        no_display,
    })
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
}
