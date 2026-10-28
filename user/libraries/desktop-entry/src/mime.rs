//! Shared MIME-info 0.21 的 XML 注册数据；只做名称 glob 判型，不探测远端文件内容。
use glob::{MatchOptions, Pattern};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

const XMLNS: &str = "http://www.freedesktop.org/standards/shared-mime-info";

#[derive(Default)]
pub(super) struct Type {
    pub(super) name: String,
    pub(super) icon: Option<String>,
    parents: Vec<String>,
}

struct Glob {
    pattern: Pattern,
    mime: String,
    weight: u8,
    sensitive: bool,
    rank: usize,
}

#[derive(Default)]
pub(super) struct Database {
    types: HashMap<String, Type>,
    aliases: HashMap<String, String>,
    globs: Vec<Glob>,
}

impl Database {
    pub(super) fn load(directories: &[PathBuf]) -> Self {
        let mut result = Self::default();
        // 低优先级先合并；用户数据和同目录 Override.xml 最后覆盖名称、图标与 glob-deleteall。
        for (rank, directory) in directories.iter().enumerate().rev() {
            let mut packages: Vec<_> = fs::read_dir(directory.join("mime/packages"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "xml"))
                .collect();
            packages.sort_by_key(|path| {
                (
                    path.file_name().is_some_and(|name| name == "Override.xml"),
                    path.clone(),
                )
            });
            for path in packages {
                let Ok(text) = fs::read_to_string(path) else {
                    continue;
                };
                let Ok(document) = roxmltree::Document::parse(&text) else {
                    continue;
                };
                if document.root_element().tag_name().namespace() != Some(XMLNS) {
                    continue;
                }
                for node in document
                    .root_element()
                    .children()
                    .filter(|node| node.has_tag_name((XMLNS, "mime-type")))
                {
                    let Some(mime) = node.attribute("type").filter(|mime| valid_mime(mime)) else {
                        continue;
                    };
                    let record = result.types.entry(mime.to_owned()).or_default();
                    for child in node.children().filter(|node| {
                        node.is_element() && node.tag_name().namespace() == Some(XMLNS)
                    }) {
                        match child.tag_name().name() {
                            "comment" => {
                                let language = child.attribute((roxmltree::NS_XML_URI, "lang"));
                                if (language.is_none() && record.name.is_empty())
                                    || language == Some("zh_CN")
                                {
                                    record.name = child.text().unwrap_or_default().to_owned();
                                }
                            }
                            "icon" => record.icon = child.attribute("name").map(str::to_owned),
                            "generic-icon" if record.icon.is_none() => {
                                record.icon = child.attribute("name").map(str::to_owned)
                            }
                            "sub-class-of" => {
                                if let Some(parent) =
                                    child.attribute("type").filter(|mime| valid_mime(mime))
                                    && !record.parents.iter().any(|value| value == parent)
                                {
                                    record.parents.push(parent.to_owned());
                                }
                            }
                            "alias" => {
                                if let Some(alias) = child.attribute("type") {
                                    result.aliases.insert(alias.to_owned(), mime.to_owned());
                                }
                            }
                            "glob-deleteall" => result.globs.retain(|glob| glob.mime != mime),
                            "glob" => {
                                if let Some(pattern) = child
                                    .attribute("pattern")
                                    .and_then(|pattern| Pattern::new(pattern).ok())
                                {
                                    result.globs.push(Glob {
                                        pattern,
                                        mime: mime.to_owned(),
                                        rank,
                                        weight: child
                                            .attribute("weight")
                                            .and_then(|s| s.parse().ok())
                                            .unwrap_or(50),
                                        sensitive: child.attribute("case-sensitive")
                                            == Some("true"),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        result
    }

    pub(super) fn mime_for(&self, path: &Path) -> Option<&str> {
        let name = path.file_name()?.to_str()?;
        self.globs
            .iter()
            .filter(|glob| {
                glob.pattern.matches_with(
                    name,
                    MatchOptions {
                        case_sensitive: glob.sensitive,
                        require_literal_separator: false,
                        require_literal_leading_dot: false,
                    },
                )
            })
            .max_by_key(|glob| {
                (
                    glob.weight,
                    glob.pattern.as_str().len(),
                    std::cmp::Reverse(glob.rank),
                )
            })
            .map(|glob| glob.mime.as_str())
    }

    pub(super) fn record(&self, mime: &str) -> Option<&Type> {
        self.types.get(self.canonical(mime))
    }

    pub(super) fn canonical<'a>(&'a self, mime: &'a str) -> &'a str {
        self.aliases.get(mime).map_or(mime, String::as_str)
    }

    pub(super) fn hierarchy<'a>(&'a self, mime: &'a str) -> Vec<&'a str> {
        let mut result = vec![self.canonical(mime)];
        let mut seen = HashSet::from([result[0]]);
        let mut index = 0;
        while index < result.len() {
            if let Some(record) = self.record(result[index]) {
                for parent in &record.parents {
                    let parent = self.canonical(parent);
                    if seen.insert(parent) {
                        result.push(parent);
                    }
                }
            }
            index += 1;
        }
        result
    }
}

fn valid_mime(value: &str) -> bool {
    value.split_once('/').is_some_and(|(kind, subtype)| {
        [kind, subtype].iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
        })
    })
}
