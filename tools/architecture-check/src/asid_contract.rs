//! ASID capability/activation ownership, IRQ transfer and noreturn resource retirement fence.
use super::SourceFile;

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let owner = "kernel/src/arch/address_space_id.rs";
    require(
        sources,
        owner,
        &[
            "reservations: Mutex<Reservations>",
            "generation: AtomicU64",
            "active: Vec<AtomicU64>",
            "capacity > 2 * cpu_count",
            "active.swap(0, Ordering::AcqRel)",
            "context.identity.store(identity, Ordering::Release)",
        ],
        errors,
    );
    for backend in ["aarch64", "riscv64"] {
        let mmu = format!("kernel/src/arch/{backend}/mmu.rs");
        require(
            sources,
            &mmu,
            &[
                "static ADDRESS_SPACE_IDS: Once<Allocator>",
                "context: Arc<AddressSpaceContext>",
                ".activate(&self.context, logical_cpu, |flush|",
                "pub(super) fn activate(&self, logical_cpu: usize)",
            ],
            errors,
        );
        let page_table = format!("kernel/src/arch/{backend}/page_table.rs");
        require(
            sources,
            &page_table,
            &[
                "address_space_context: Option<Arc<AddressSpaceContext>>",
                "AddressSpaceContext::try_new()",
                "context.retire_after_global_fence()",
            ],
            errors,
        );
        let trap = format!("kernel/src/arch/{backend}/trap.rs");
        let Some(source) = sources.iter().find(|source| source.relative == trap) else {
            errors.push(format!("{trap}: ASID activation caller missing"));
            continue;
        };
        let body = source
            .text
            .split("pub(crate) fn return_to_user(")
            .nth(1)
            .unwrap_or("");
        let activation = body.find("let encoded = address_space.activate(");
        let retirement = body.find("drop(address_space);");
        let handoff = body.find("options(noreturn)");
        if !matches!((activation, retirement, handoff), (Some(a), Some(r), Some(h)) if a < r && r < h)
            || body.contains("address_space.encoded()")
        {
            errors.push(format!(
                "{trap}: resolve activation scalar and drop token Arc before noreturn"
            ));
        }
        if body.contains("enable_scheduler_interrupts") || body.contains("restore_local") {
            errors.push(format!(
                "{trap}: ASID activation/hardware installation must remain IRQ-disabled"
            ));
        }
    }
    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("kernel/src/arch/"))
    {
        if source.text.contains("allocate_address_space_id")
            || source.text.contains("AddressSpaceIdentifiersExhausted")
            || source.text.contains("ADDRESS_SPACE_ID_CPUS")
        {
            errors.push(format!(
                "{}: eager finite ASID path competes with generation owner",
                source.relative
            ));
        }
    }
    if let Some(trap) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/trap/mod.rs")
    {
        let body = trap
            .text
            .split("pub(crate) fn trap_return()")
            .nth(1)
            .unwrap_or("");
        let preparation = body.find("delivery_task.prepare_signal_delivery()");
        let masking = body.find("arch::interrupt::disable_for_transfer();");
        let snapshot = body.find("let user_address_space = current_task.user_token();");
        if !matches!((preparation, masking, snapshot), (Some(p), Some(m), Some(s)) if p < m && m < s)
        {
            errors.push("trap_return: final IRQ mask must follow blocking signal preparation and precede root snapshot".into());
        }
    }
    if let Some(trap) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/trap/mod.rs")
    {
        let body = trap
            .text
            .split("pub(crate) fn trap_return()")
            .nth(1)
            .unwrap_or("")
            .split("pub(crate) fn handle_kernel_trap()")
            .next()
            .unwrap_or("");
        let after_mask = body
            .split("arch::interrupt::disable_for_transfer();")
            .nth(1)
            .unwrap_or("");
        if body
            .matches("arch::interrupt::disable_for_transfer();")
            .count()
            != 1
            || [
                "prepare_signal_delivery",
                "copy_to_user",
                "copy_from_user",
                "memory_set.lock",
                "suspend_current",
            ]
            .iter()
            .any(|operation| after_mask.contains(operation))
        {
            errors.push("trap_return: one final transfer window must remain nonblocking".into());
        }
    }
    if let Some(boot) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/main.rs")
    {
        let topology = boot.text.find("cpu::initialize(");
        let allocator = boot
            .text
            .find("arch::mmu::initialize_asid_allocator(cpu::count());");
        let roots = boot.text.find("memory::init(");
        if !matches!((topology, allocator, roots), (Some(t), Some(a), Some(r)) if t < a && a < r) {
            errors
                .push("main: ASID storage must be prepared after topology and before roots".into());
        }
    }
}

fn require(sources: &[SourceFile], path: &str, fragments: &[&str], errors: &mut Vec<String>) {
    let Some(source) = sources.iter().find(|source| source.relative == path) else {
        errors.push(format!("{path}: ASID capability owner missing"));
        return;
    };
    for fragment in fragments {
        if !source.text.contains(fragment) {
            errors.push(format!("{path}: ASID capability lost `{fragment}`"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_keeps_generation_owner_and_closes_noreturn_handoff() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
    #[test]
    fn rejects_token_arc_leak_and_raw_encoding_reread() {
        for backend in ["aarch64", "riscv64"] {
            let path = format!("kernel/src/arch/{backend}/trap.rs");
            let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
            let trap = sources
                .iter_mut()
                .find(|source| source.relative == path)
                .unwrap();
            trap.text = trap.text.replace("drop(address_space);", "");
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("drop token Arc before noreturn")),
                "{errors:#?}"
            );
        }
    }
    #[test]
    fn rejects_irq_mask_only_before_blocking_signal_preparation() {
        let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let trap = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/trap/mod.rs")
            .unwrap();
        trap.text = trap
            .text
            .replace("arch::interrupt::disable_for_transfer();", "")
            .replace(
                "pub(crate) fn trap_return() -> ! {",
                "pub(crate) fn trap_return() -> ! {\narch::interrupt::disable_for_transfer();",
            );
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("final IRQ mask must follow blocking")),
            "{errors:#?}"
        );
    }
    #[test]
    fn rejects_sleepable_signal_operation_inside_final_transfer_window() {
        let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let trap = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/trap/mod.rs")
            .unwrap();
        trap.text = trap.text.replace("let user_address_space = current_task.user_token();",
            "let _ = current_task.prepare_signal_delivery();\nlet user_address_space = current_task.user_token();");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("must remain nonblocking")),
            "{errors:#?}"
        );
    }
    #[test]
    fn rejects_number_claim_during_root_creation() {
        let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let page_table = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/arch/riscv64/page_table.rs")
            .unwrap();
        page_table
            .text
            .push_str("\nfn allocate_address_space_id() {}\n");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("eager finite ASID path")),
            "{errors:#?}"
        );
    }
}
