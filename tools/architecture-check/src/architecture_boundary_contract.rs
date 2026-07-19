use std::{fs, path::Path};

use super::SourceFile;

/// @description 检查静态 arch/platform façade、raw ABI 与 target dependency containment。
/// @param root 定位 manifest 与 retired paths；sources 是统一源码快照；errors 接收违规。
/// @return 无；全部违规一次收集。
/// @errors 源码、路径或 manifest 违规均追加到 errors。
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
        "kernel/src/arch/riscv64/hart.rs",
        "kernel/src/arch/aarch64/fp_instruction.rs",
        "kernel/src/task/context.rs",
        "kernel/src/task/trap_context.rs",
        "kernel/src/drivers/platform.rs",
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
