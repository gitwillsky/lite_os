//! Kernel 领域与源码布局的唯一对应；目录分组不合并依赖 owner。
use super::SourceFile;
use std::path::Path;
use syn::Item;

/// (稳定领域名, 规范 Rust 命名空间)；命名空间同时确定 kernel/src 下的物理位置。
pub(super) const DOMAINS: &[(&str, &str)] = &[
    ("arch", "arch"),
    ("audio", "devices::audio"),
    ("block", "devices::block"),
    ("boot_screen", "boot::boot_screen"),
    ("cmdline", "boot::cmdline"),
    ("config", "config"),
    ("cpu", "execution::cpu"),
    ("crash", "crash"),
    ("deferred", "execution::deferred"),
    ("drivers", "drivers"),
    ("device", "devices::model"),
    ("pci", "drivers::pci"),
    ("usb", "drivers::usb"),
    ("virtio", "drivers::virtio"),
    ("drm", "devices::drm"),
    ("entry", "execution::entry"),
    ("fallible_tree", "fallible_tree"),
    ("file", "file"),
    ("fs", "fs"),
    ("id", "id"),
    ("input", "devices::input"),
    ("io_uring", "io_uring"),
    ("ipc", "ipc"),
    ("lang_item", "lang_item"),
    ("log", "log"),
    ("main", "main"),
    ("memory", "memory"),
    ("namespace", "namespace"),
    ("platform", "platform"),
    ("random", "random"),
    ("net", "net"),
    ("sync", "execution::sync"),
    ("syscall", "syscall"),
    ("system", "system"),
    ("task", "execution::task"),
    ("timer", "execution::timer"),
    ("trap", "execution::trap"),
    ("tty", "devices::tty"),
    ("virtio_port", "devices::virtio_port"),
    ("boot", "boot"),
    ("devices", "devices"),
    ("execution", "execution"),
    ("interrupt", "execution::interrupt"),
    ("console", "devices::console"),
    ("device_registry", "devices::registry"),
];
const GROUPS: &[&str] = &["boot", "devices", "drivers", "execution"];

pub(super) fn names() -> impl Iterator<Item = &'static str> {
    DOMAINS.iter().map(|entry| entry.0)
}
pub(super) fn known(name: &str) -> bool {
    names().any(|owner| owner == name)
}
pub(super) fn namespace_owner(path: &[String]) -> Option<&'static str> {
    DOMAINS
        .iter()
        .filter(|(_, namespace)| {
            let names: Vec<_> = namespace.split("::").collect();
            path.len() >= names.len() && names.iter().zip(path).all(|(a, b)| *a == b)
        })
        .max_by_key(|(_, namespace)| namespace.len())
        .map(|(name, _)| *name)
}
/// 已分组的领域不得重新在 crate 根建立别名，否则条件编译源码可绕过正向依赖检查。
pub(super) fn obsolete_root(name: &str) -> bool {
    !DOMAINS.iter().any(|(_, namespace)| *namespace == name)
        && DOMAINS
            .iter()
            .any(|(_, namespace)| namespace.rsplit("::").next() == Some(name))
}
pub(super) fn grouping(path: &[String]) -> bool {
    path.len() == 1 && GROUPS.contains(&path[0].as_str())
}
/// 审核物理目录、分组入口与领域清单，新增领域不能隐含归入一个大目录绕过依赖检查。
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("kernel/src/"))
    {
        if !known(&source.owner) {
            errors.push(format!(
                "{}: kernel source has no registered domain",
                source.relative
            ));
            continue;
        }
        if GROUPS.contains(&source.owner.as_str()) {
            let expected = format!("kernel/src/{}/mod.rs", source.owner);
            if source.relative != expected {
                errors.push(format!(
                    "{}: grouping directory requires an explicit child domain",
                    source.relative
                ));
                continue;
            }
            for item in &source.syntax.items {
                match item {
                    Item::Mod(module)
                        if module.content.is_none()
                            && DOMAINS.iter().any(|(_, namespace)| {
                                *namespace == format!("{}::{}", source.owner, module.ident)
                            }) => {}
                    _ => errors.push(format!(
                        "{}: a grouping entry only declares registered child domains",
                        source.relative
                    )),
                }
            }
        }
    }
    // 文件与目录使用同一封闭清单；不把不在源码加载清单里的孤立根文件遗漏掉。
    if let Ok(entries) = std::fs::read_dir(root.join("kernel/src")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.path().is_dir() && !name.ends_with(".rs") {
                continue;
            }
            let name = name.strip_suffix(".rs").unwrap_or(&name);
            if !DOMAINS
                .iter()
                .any(|(_, namespace)| namespace.split("::").next() == Some(name))
            {
                errors.push(format!("kernel/src/{name}: unregistered root entry"));
            }
        }
    }
}
