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
    if !bootstrap_wfi_has_exact_trap_resume(&source) {
        errors.push(format!(
            "{TRAP_SOURCE}: bootstrap external WFI must enable SIE and make trap return skip an already-acknowledged WFI"
        ));
    }
    if !source.contains("    .align 2\n__kernel_trap:") {
        errors.push(format!(
            "{TRAP_SOURCE}: __kernel_trap must be 4-byte aligned before publication through stvec"
        ));
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

fn bootstrap_wfi_has_exact_trap_resume(source: &str) -> bool {
    let wait = source.find("__wait_for_external_interrupt:");
    let software_enable = source.find("csrsi sie, 2");
    let enable = source.find("csrsi sstatus, 2");
    let sleep = source.find("__bootstrap_external_wfi:\n    wfi");
    let resume = source.find("__bootstrap_external_wfi_resume:");
    let disable = source.find("csrci sstatus, 2");
    let software_disable = source.find("csrci sie, 2");
    let trap = source
        .find("__kernel_trap:")
        .zip(source.find("call __liteos_kernel_trap"))
        .map(|(start, end)| &source[start..end]);
    wait.zip(software_enable)
        .zip(enable)
        .zip(sleep)
        .zip(resume)
        .zip(disable)
        .zip(software_disable)
        .is_some_and(
            |(
                (((((wait, software_enable), enable), sleep), resume), disable),
                software_disable,
            )| {
                wait < software_enable
                    && software_enable < enable
                    && enable < sleep
                    && sleep < resume
                    && resume < disable
                    && disable < software_disable
            },
        )
        && trap.is_some_and(|trap| {
            trap.contains("la t1, __bootstrap_external_wfi")
                && trap.contains("la t0, __bootstrap_external_wfi_resume")
                && trap.contains("csrw sepc, t0")
        })
}

fn bootstrap_interrupt_state_precedes_devices(root: &Path) -> bool {
    let Ok(main) = fs::read_to_string(root.join("kernel/src/main.rs")) else {
        return false;
    };
    main.find("task::initialize_interrupt_state();")
        .zip(main.find("platform::initialize_devices();"))
        .zip(main.find("task::init("))
        .is_some_and(|((interrupt_state, devices), task)| {
            interrupt_state < devices && devices < task
        })
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
    fn repository_bootstrap_wfi_cannot_reenter_after_ack() {
        let root = super::super::repository_root();
        let source = std::fs::read_to_string(root.join(super::TRAP_SOURCE)).expect("trap assembly");
        assert!(super::bootstrap_wfi_has_exact_trap_resume(&source));
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
}
