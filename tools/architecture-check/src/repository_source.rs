use std::{
    fs,
    path::{Path, PathBuf},
};

use super::SourceFile;

#[derive(Clone, Copy)]
struct SourceDomain {
    root: &'static str,
    binary_crate: bool,
}

const SOURCE_DOMAINS: &[SourceDomain] = &[
    SourceDomain {
        root: "kernel/src",
        binary_crate: true,
    },
    SourceDomain {
        root: "bootloader/src",
        binary_crate: true,
    },
];

/// @description 返回 architecture-check 实际加载的源码根，避免目录约束复制 domain 清单。
/// @return 按权威 domain 清单顺序产生源码根的 iterator。
/// @errors 无错误。
pub(super) fn source_roots() -> impl Iterator<Item = &'static str> {
    SOURCE_DOMAINS.iter().map(|domain| domain.root)
}

/// @description 定位 architecture-check 所属 workspace 根目录。
/// @return 两级父目录归一化后的 workspace 根。
/// @errors 布局不满足 `tools/architecture-check` 不变量时 fail-stop。
pub(super) fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("architecture-check must live under tools/")
        .to_path_buf()
}

/// @description 递归收集目录中的 Rust 源文件。
/// @param directory 待扫描根；files 接收发现的路径。
/// @return 完整遍历后返回 unit。
/// @errors 任一目录或 entry 不可读时返回带路径的错误。
pub(super) fn rust_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("failed to read directory entry: {error}"))?
            .path();
        if path.is_dir() {
            rust_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// @description 从所有权清单加载、解析并标注 architecture-check 的生产源码输入。
/// @param root workspace 根目录。
/// @return 排序、解析并带 module owner 的统一源码快照。
/// @errors 读取、解析或相对路径归一化失败时返回错误。
pub(super) fn load_sources(root: &Path) -> Result<Vec<SourceFile>, String> {
    let mut sources = Vec::new();
    for domain in SOURCE_DOMAINS {
        let source_root = root.join(domain.root);
        let mut paths = Vec::new();
        rust_files(&source_root, &mut paths)?;
        paths.sort();
        for path in paths {
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            let syntax = syn::parse_file(&text)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
            let relative_path = path
                .strip_prefix(root)
                .map_err(|error| format!("invalid source path {}: {error}", path.display()))?;
            let owner = module_owner(relative_path, domain.root);
            sources.push(SourceFile {
                relative: relative_path.to_string_lossy().replace('\\', "/"),
                owner,
                lines: text.lines().map(str::to_owned).collect(),
                text,
                syntax,
                binary_crate: domain.binary_crate,
            });
        }
    }
    Ok(sources)
}

fn module_owner(relative: &Path, source_root: &str) -> String {
    let inside = relative
        .strip_prefix(source_root)
        .expect("source path must be below its source root");
    let first = inside
        .components()
        .next()
        .expect("Rust source path is not empty");
    let name = first.as_os_str().to_string_lossy();
    name.strip_suffix(".rs").unwrap_or(&name).to_owned()
}
