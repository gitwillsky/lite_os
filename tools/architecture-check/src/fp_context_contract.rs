use std::{fs, path::Path};

const TRAP_SOURCE: &str = "kernel/src/arch/riscv64/trap.S";

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    let path = root.join(TRAP_SOURCE);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            errors.push(format!("{TRAP_SOURCE}: failed to read assembly: {error}"));
            return;
        }
    };
    if !user_state_precedes_kernel_fp_enable(&source) {
        errors.push(format!(
            "{TRAP_SOURCE}: user sstatus must be saved before FS=Dirty kernel ownership is published, and kernel FP must be enabled before entering Rust"
        ));
    }
    if !guarded_wfi_has_exact_trap_resume(&source) {
        errors.push(format!(
            "{TRAP_SOURCE}: idle guarded WFI must enable SIE and make trap return skip an already-acknowledged WFI"
        ));
    }
    if !source.contains("    .align 2\n__kernel_trap:") {
        errors.push(format!(
            "{TRAP_SOURCE}: __kernel_trap must be 4-byte aligned before publication through stvec"
        ));
    }
    let arm = fs::read_to_string(root.join("kernel/src/arch/aarch64/trap.S")).unwrap_or_default();
    if !privileged_user_address_isolation(&arm) {
        errors.push(
            "AArch64: TTBR0 isolation must precede Rust and authorized restore must precede eret"
                .into(),
        );
    }
    if !bootstrap_interrupt_state_precedes_devices(root) {
        errors.push(
            "kernel/src/main.rs: membarrier interrupt state must precede device initialization and task loading"
                .to_string(),
        );
    }
}

fn user_state_precedes_kernel_fp_enable(source: &str) -> bool {
    let Some(entry) = source.find("__alltraps:") else {
        return false;
    };
    let Some(end) = source[entry..].find("__kernel_trap:") else {
        return false;
    };
    let entry = &source[entry..entry + end];
    entry
        .find("sd t0, 32*8(sp)")
        .zip(entry.find("csrs sstatus, t2"))
        .zip(entry.find("jr t1"))
        .is_some_and(|((save_user_status, enable_kernel_fp), enter_kernel)| {
            save_user_status < enable_kernel_fp && enable_kernel_fp < enter_kernel
        })
}

fn guarded_wfi_has_exact_trap_resume(source: &str) -> bool {
    let wait = source.find("__wait_with_local_irq_masked:");
    let enable = source.find("csrsi sstatus, 2");
    let sleep = source.find("__local_irq_wait_wfi:\n    wfi");
    let resume = source.find("__local_irq_wait_wfi_resume:");
    let disable = source.find("csrci sstatus, 2");
    let trap = source
        .find("__kernel_trap:")
        .zip(source.find("call __liteos_kernel_trap"))
        .map(|(start, end)| &source[start..end]);
    wait.zip(enable)
        .zip(sleep)
        .zip(resume)
        .zip(disable)
        .is_some_and(|((((wait, enable), sleep), resume), disable)| {
            wait < enable && enable < sleep && sleep < resume && resume < disable
        })
        && trap.is_some_and(|trap| {
            trap.contains("la t1, __local_irq_wait_wfi")
                && trap.contains("la t0, __local_irq_wait_wfi_resume")
                && trap.contains("csrw sepc, t0")
        })
}

fn bootstrap_interrupt_state_precedes_devices(root: &Path) -> bool {
    let Ok(main) = fs::read_to_string(root.join("kernel/src/main.rs")) else {
        return false;
    };
    main.find("execution::barrier::initialize();")
        .zip(main.find("platform::initialize_devices();"))
        .zip(main.find("bootstrap::start_component_manager("))
        .is_some_and(|((interrupt_state, devices), task)| {
            interrupt_state < devices && devices < task
        })
}

fn privileged_user_address_isolation(source: &str) -> bool {
    let entry = source
        .split("__aarch64_user_common:")
        .nth(1)
        .unwrap_or("")
        .split("__aarch64_kernel_sync:")
        .next()
        .unwrap_or("");
    let restore = source.split("__aarch64_restore:").nth(1).unwrap_or("");
    matches!((entry.find("mrs x10, ttbr1_el1"), entry.find("msr ttbr0_el1, x10"), entry.find("isb"), entry.find("br x13")), (Some(root), Some(install), Some(barrier), Some(rust)) if root < install && install < barrier && barrier < rust)
        && matches!((restore.find("msr ttbr0_el1, x1"), restore.find("isb"), restore.find("eret")), (Some(install), Some(barrier), Some(ret)) if install < barrier && barrier < ret)
}

#[cfg(test)]
mod tests {
    #[test]
    fn repository_user_trap_publishes_kernel_fp_ownership() {
        let root = super::super::repository_root();
        let source = std::fs::read_to_string(root.join(super::TRAP_SOURCE)).expect("trap assembly");
        assert!(super::user_state_precedes_kernel_fp_enable(&source));
    }

    #[test]
    fn repository_guarded_wfi_cannot_reenter_after_ack() {
        let root = super::super::repository_root();
        let source = std::fs::read_to_string(root.join(super::TRAP_SOURCE)).expect("trap assembly");
        assert!(super::guarded_wfi_has_exact_trap_resume(&source));
    }

    #[test]
    fn repository_initializes_software_trap_state_before_devices() {
        let root = super::super::repository_root();
        assert!(super::bootstrap_interrupt_state_precedes_devices(&root));
    }

    #[test]
    fn repository_kernel_trap_entry_satisfies_stvec_alignment() {
        let root = super::super::repository_root();
        let source = std::fs::read_to_string(root.join(super::TRAP_SOURCE)).expect("trap assembly");
        assert!(source.contains("    .align 2\n__kernel_trap:"));
    }
    #[test]
    fn user_address_isolation_cannot_be_removed_from_entry_or_return() {
        let root = super::super::repository_root();
        let source = std::fs::read_to_string(root.join("kernel/src/arch/aarch64/trap.S")).unwrap();
        assert!(super::privileged_user_address_isolation(&source));
        for instruction in ["msr ttbr0_el1, x10", "msr ttbr0_el1, x1\n"] {
            assert!(!super::privileged_user_address_isolation(
                &source.replace(instruction, "")
            ));
        }
    }
}
