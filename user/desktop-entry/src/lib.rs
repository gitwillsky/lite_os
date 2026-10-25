//! freedesktop Desktop Entry 子集：读取已安装程序的名称、图标、分类与命令；不把 Exec 当成 shell 程序解释。
use std::{fs, path::Path, process::Command};

/// 一个可启动的程序条目。
pub struct Entry {
    pub name: String,
    /// `Icon=` 的名称（不是路径），没有时为 `None`。
    pub icon: Option<String>,
    categories: Vec<String>,
    arguments: Vec<String>,
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

    pub fn in_category(&self, category: &str) -> bool {
        self.categories.iter().any(|c| c == category)
    }
}

/// 读取目录里的全部 `.desktop` 条目，按名称排序；目录不存在或条目无效时跳过。
pub fn load(directory: &Path) -> Vec<Entry> {
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
    let mut executable = None;
    let mut icon = None;
    let mut categories = Vec::new();
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
            "Icon" => icon = Some(value.to_owned()),
            "Categories" => {
                categories = value
                    .split(';')
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            "Type" => application = value == "Application",
            "Exec" => executable = Some(arguments(value)?),
            "Hidden" | "NoDisplay" | "Terminal" if value == "true" => return None,
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
        categories,
        arguments,
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
