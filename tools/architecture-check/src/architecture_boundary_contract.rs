use std::{fs, path::Path};

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;

use super::SourceFile;

// 检查标识符而非文本子串：覆盖分组/别名导入、指针方法和宏体，忽略注释与字符串。
fn contains_identifier(tokens: TokenStream, forbidden: impl Fn(&str) -> bool + Copy) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(ident) => forbidden(&ident.to_string()),
        TokenTree::Group(group) => contains_identifier(group.stream(), forbidden),
        _ => false,
    })
}

fn raw_mmio(name: &str) -> bool {
    name.starts_with("read_mmio_") || name.starts_with("write_mmio_")
}

/// 检查静态 arch/platform façade、raw ABI 与 target dependency containment。
///
/// # Parameters
///
/// - `root`: 定位 manifest 与 retired paths；sources 是统一源码快照；errors 接收违规。
///
/// # Returns
///
/// 无；全部违规一次收集。
///
/// # Errors
///
/// 源码、路径或 manifest 违规均追加到 errors。
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("kernel/src/"))
    {
        if source.owner != "arch"
            && (source.text.contains("riscv::") || source.text.contains("use riscv::{"))
        {
            errors.push(format!(
                "{}: direct RISC-V mechanism is restricted to the arch backend",
                source.relative
            ));
        }
        if !matches!(source.owner.as_str(), "arch" | "platform")
            && source.text.contains("target_arch")
        {
            errors.push(format!(
                "{}: target selection is restricted to static arch/platform facades",
                source.relative
            ));
        }
        if !matches!(source.owner.as_str(), "arch" | "hal")
            && contains_identifier(source.syntax.to_token_stream(), raw_mmio)
        {
            errors.push(format!(
                "{}: device register access must go through hal::MmioBus, not the raw arch MMIO primitives",
                source.relative
            ));
        }
        if (source.owner == "platform" || source.relative.starts_with("kernel/src/devices/"))
            && contains_identifier(source.syntax.to_token_stream(), |name| {
                matches!(name, "read_volatile" | "write_volatile")
            })
        {
            errors.push(format!(
                "{}: platform/devices registers must use hal::MmioBus (bounds, alignment and the arch-fixed access form), not raw volatile pointers",
                source.relative
            ));
        }
        if source.owner != "arch" && source.text.contains("crate::arch::riscv64") {
            errors.push(format!(
                "{}: concrete architecture paths may not cross the arch facade",
                source.relative
            ));
        }
        if !matches!(source.owner.as_str(), "arch" | "platform")
            && (source.text.contains("core::arch::asm") || source.text.contains("asm!("))
        {
            errors.push(format!(
                "{}: inline assembly is restricted to the arch backend",
                source.relative
            ));
        }
        if source.owner != "arch"
            && (source.text.contains("RiscvPteFlags") || source.text.contains("PageTableFlags"))
        {
            errors.push(format!(
                "{}: encoded page-table flags may not cross the semantic MMU facade",
                source.relative
            ));
        }
        if source.owner != "platform" && source.text.contains("crate::platform::qemu_virt") {
            errors.push(format!(
                "{}: concrete machine paths may not cross the platform facade",
                source.relative
            ));
        }
        if source.owner != "platform" && source.text.contains("PlatformInfo") {
            errors.push(format!(
                "{}: concrete platform discovery records may not cross the platform facade",
                source.relative
            ));
        }
        if !matches!(
            source.owner.as_str(),
            "arch" | "cpu" | "entry" | "main" | "platform"
        ) && source.text.contains("HardwareCpuId")
        {
            errors.push(format!(
                "{}: hardware CPU identity may not enter generic kernel domains",
                source.relative
            ));
        }
        if source.relative == "kernel/src/main.rs"
            && (source.text.contains("no_mangle") || source.text.contains("extern \"C\""))
        {
            errors.push(
                "kernel/src/main.rs: raw boot/trap ABI must remain behind typed architecture seams"
                    .to_owned(),
            );
        }
        if !matches!(source.owner.as_str(), "arch" | "entry") && source.text.contains("no_mangle") {
            errors.push(format!(
                "{}: raw exported symbols are restricted to architecture/entry codecs",
                source.relative
            ));
        }
        if source.text.contains("dyn Architecture") || source.text.contains("trait Architecture") {
            errors.push(format!(
                "{}: runtime architecture dispatch is forbidden; use the static arch facade",
                source.relative
            ));
        }
        for retired_capability in [
            "SUPPORTS_RISCV_HWPROBE",
            "supports_riscv_hwprobe",
            "from_controller(kind:",
            "activate_user_floating_point",
            "fn kernel_stack_user_context",
            "crate::arch::context::is_kernel_stack_user_context",
        ] {
            if source.text.contains(retired_capability) {
                errors.push(format!(
                    "{}: retired runtime architecture capability/private dispatch {:?} must not return",
                    source.relative, retired_capability
                ));
            }
        }
    }

    for retired in [
        "kernel/src/hardware/arch/riscv64/hart.rs",
        "kernel/src/hardware/arch/aarch64/fp_instruction.rs",
        "kernel/src/task/context.rs",
        "kernel/src/task/trap_context.rs",
        "kernel/src/devices/drivers/platform.rs",
    ] {
        if root.join(retired).exists() {
            errors.push(format!(
                "{retired}: retired architecture path must not be restored"
            ));
        }
    }

    let manifest = fs::read_to_string(root.join("kernel/Cargo.toml")).unwrap_or_default();
    let Some(target_dependencies) =
        manifest.find("[target.'cfg(target_arch = \"riscv64\")'.dependencies]")
    else {
        errors.push(
            "kernel/Cargo.toml: RISC-V dependencies require a target-specific table".to_owned(),
        );
        return;
    };
    if manifest[..target_dependencies]
        .lines()
        .any(|line| line.trim_start().starts_with("riscv ="))
    {
        errors.push(
            "kernel/Cargo.toml: riscv crate must not be an unconditional dependency".to_owned(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(owner: &str, relative: &str, text: &str) -> SourceFile {
        SourceFile {
            relative: relative.to_owned(),
            owner: owner.to_owned(),
            text: text.to_owned(),
            lines: text.lines().map(str::to_owned).collect(),
            syntax: syn::parse_file(text).expect("fixture must parse"),
            binary_crate: true,
        }
    }

    fn violations(owner: &str, relative: &str, text: &str) -> Vec<String> {
        let mut errors = Vec::new();
        check(
            Path::new("/nonexistent"),
            &[source(owner, relative, text)],
            &mut errors,
        );
        errors
            .into_iter()
            .filter(|error| error.contains("MmioBus"))
            .collect()
    }

    const PLATFORM: &str = "kernel/src/hardware/platform/qemu_virt/x.rs";
    const DEVICE: &str = "kernel/src/devices/virtio/x.rs";

    #[test]
    fn register_access_through_hal_and_documentation_are_accepted() {
        for (owner, path) in [("platform", PLATFORM), ("virtio", DEVICE)] {
            for text in [
                "fn f(bus: &crate::hal::MmioBus) { let _ = bus.read_u32(0); }",
                "// read_volatile must stay behind HAL\nfn f() { let _ = \"arch::read_mmio_u32\"; }",
            ] {
                assert!(violations(owner, path, text).is_empty());
            }
        }
        assert!(
            violations(
                "hal",
                "kernel/src/hardware/hal/bus.rs",
                "fn f() { unsafe { crate::arch::read_mmio_u32(0) }; }"
            )
            .is_empty()
        );
    }

    #[test]
    fn raw_register_access_cannot_hide_in_imports_methods_or_macros() {
        for (owner, path) in [("platform", PLATFORM), ("virtio", DEVICE)] {
            for text in [
                "fn f(p: *const u32) { unsafe { core::ptr::read_volatile(p) }; }",
                "fn f(p: *mut u32) { unsafe { p.write_volatile(1) }; }",
                "use core::ptr::{read_volatile as load};",
                "use crate::arch::{read_mmio_u32 as load};",
                "use crate::arch::{write_mmio_u64};",
                "macro_rules! load { ($p:expr) => { unsafe { $p.read_volatile() } }; }",
            ] {
                assert_eq!(violations(owner, path, text).len(), 1, "{owner}: {text}");
            }
            for width in [8, 16, 32, 64] {
                for access in ["read", "write"] {
                    let text = format!("use crate::arch::{{{access}_mmio_u{width} as access}};");
                    assert_eq!(violations(owner, path, &text).len(), 1);
                }
            }
        }
    }
}
