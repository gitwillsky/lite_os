use std::{fs, path::Path};

const LINKER: &str = "kernel/linkers/aarch64.ld";
const START: &str = "kernel/src/arch/aarch64/start.rs";
const TRAP: &str = "kernel/src/arch/aarch64/trap.S";
const TRAP_RS: &str = "kernel/src/arch/aarch64/trap.rs";
const INTERRUPT: &str = "kernel/src/arch/aarch64/interrupt.rs";
const ADDRESS: &str = "kernel/src/memory/address.rs";
const MMU: &str = "kernel/src/arch/aarch64/mmu.rs";
const RISCV_MMU: &str = "kernel/src/arch/riscv64/mmu.rs";
const VA39: &str = "kernel/src/arch/aarch64/va39.rs";
const STARTUP: &str = "kernel/src/arch/aarch64/startup.rs";
const INSTRUCTION_CACHE: &str = "kernel/src/arch/aarch64/instruction_cache.rs";
const SHOOTDOWN: &str = "kernel/src/memory/mm/shootdown.rs";
const GICV3: &str = "kernel/src/platform/qemu_virt/aarch64/gicv3.rs";
const AARCH64_IO: &str = "kernel/src/arch/aarch64/io.rs";
const MMIO_BUS: &str = "kernel/src/drivers/hal/bus.rs";
const HEAP: &str = "kernel/src/memory/heap_allocator.rs";
const VIRTIO_QUEUE: &str = "kernel/src/drivers/virtio_queue.rs";
const KERNEL_STACK: &str = "kernel/src/memory/kernel_stack.rs";
const AARCH64_CONTEXT: &str = "kernel/src/arch/aarch64/user_context.rs";
const RISCV64_CONTEXT: &str = "kernel/src/arch/riscv64/user_context.rs";
const TASK_TRAP_CONTEXT: &str = "kernel/src/task/model/trap_context.rs";
const TASK_MODEL: &str = "kernel/src/task/model.rs";
const PROCESS_CLONE: &str = "kernel/src/task/model/process_clone.rs";
const PROCESS_EXEC: &str = "kernel/src/task/model/process_exec.rs";

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    let Ok(linker) = read(root, LINKER, errors) else {
        return;
    };
    let Ok(start) = read(root, START, errors) else {
        return;
    };
    let Ok(trap) = read(root, TRAP, errors) else {
        return;
    };
    let Ok(trap_rs) = read(root, TRAP_RS, errors) else {
        return;
    };
    let Ok(interrupt) = read(root, INTERRUPT, errors) else {
        return;
    };
    let Ok(address) = read(root, ADDRESS, errors) else {
        return;
    };
    let Ok(mmu) = read(root, MMU, errors) else {
        return;
    };
    let Ok(riscv_mmu) = read(root, RISCV_MMU, errors) else {
        return;
    };
    let Ok(va39) = read(root, VA39, errors) else {
        return;
    };
    let Ok(startup) = read(root, STARTUP, errors) else {
        return;
    };
    let Ok(instruction_cache) = read(root, INSTRUCTION_CACHE, errors) else {
        return;
    };
    let Ok(shootdown) = read(root, SHOOTDOWN, errors) else {
        return;
    };
    let Ok(gicv3) = read(root, GICV3, errors) else {
        return;
    };
    let Ok(aarch64_io) = read(root, AARCH64_IO, errors) else {
        return;
    };
    let Ok(mmio_bus) = read(root, MMIO_BUS, errors) else {
        return;
    };
    let Ok(heap) = read(root, HEAP, errors) else {
        return;
    };
    let Ok(virtio_queue) = read(root, VIRTIO_QUEUE, errors) else {
        return;
    };
    let Ok(kernel_stack) = read(root, KERNEL_STACK, errors) else {
        return;
    };
    let Ok(aarch64_context) = read(root, AARCH64_CONTEXT, errors) else {
        return;
    };
    let Ok(riscv64_context) = read(root, RISCV64_CONTEXT, errors) else {
        return;
    };
    let Ok(task_trap_context) = read(root, TASK_TRAP_CONTEXT, errors) else {
        return;
    };
    let Ok(task_model) = read(root, TASK_MODEL, errors) else {
        return;
    };
    let Ok(process_clone) = read(root, PROCESS_CLONE, errors) else {
        return;
    };
    let Ok(process_exec) = read(root, PROCESS_EXEC, errors) else {
        return;
    };

    if !(linker.contains("DIRECT_MAP_BASE = 0xffffffc000000000")
        && linker.contains(".boot")
        && linker.contains(".text : AT(ADDR(.text) - DIRECT_MAP_BASE)"))
    {
        errors.push(
            "AArch64 linker must keep a low Image boot section and high-VMA/low-LMA text".into(),
        );
    }
    if !(start.contains("msr     ttbr1_el1")
        && start.contains("__boot_ttbr1")
        && start.contains(".quad   __liteos_high_entry"))
    {
        errors.push(
            "AArch64 low entry must install static TTBR1 and branch to the high entry".into(),
        );
    }
    if trap.matches("msr ttbr0_el1").count() != 1 {
        errors.push(
            "AArch64 trap assembly may write TTBR0 only once, in the destination-user restore path"
                .into(),
        );
    }
    if !bootstrap_wfi_has_exact_irq_resume(&trap, &interrupt) {
        errors.push(
            "AArch64 bootstrap external WFI must make kernel IRQ return skip an already-acknowledged WFI"
                .into(),
        );
    }
    let restore_ttbr = trap.find("msr ttbr0_el1, x1");
    let restore_vbar = trap.find("msr vbar_el1, x2");
    if !matches!((restore_ttbr, restore_vbar), (Some(ttbr), Some(vbar)) if ttbr < vbar)
        || trap_rs.contains("__aarch64_restore as *const () as usize -")
        || !trap_rs.contains("restore = in(reg) __aarch64_restore as *const () as usize")
        || !trap_rs.contains("in(\"x2\") trampoline_address")
    {
        errors.push(
            "AArch64 user return must execute the linked TTBR1 restore entry and install TTBR0 before low VBAR"
                .into(),
        );
    }
    if trap.to_ascii_lowercase().contains("contextidr_el1")
        || trap.matches("add x11, sp, #48").count() != 1
        || trap.contains("ldr x11, [sp")
        || trap.contains("str x16, [sp")
        || !trap_rs.contains("is_kernel_stack_user_context(context_address)")
        || trap_rs.contains("context_address < USER_ADDRESS_END")
    {
        errors.push(
            "AArch64 trap context must use one fixed SP_EL1-relative ADD with no metadata pointer or CONTEXTIDR_EL1 state"
                .into(),
        );
    }
    if !address.contains("crate::arch::mmu::physical_to_virtual(self.0) as *") {
        errors.push(
            "physical-address dereference must pass through the architecture direct map".into(),
        );
    }
    if !(va39.contains("const TLBI_VIRTUAL_PAGE_MASK: u64 = (1u64 << 44) - 1")
        && va39.contains("tlbi_all_asid_operand(virtual_address: usize)")
        && va39.contains("high_half_tlbi_operand_contains_only_va_55_through_12")
        && mmu.matches("va39::tlbi_all_asid_operand(address)").count() == 2)
    {
        errors.push(
            "AArch64 TLBI operands must mask canonical VA down to architected VA[55:12]".into(),
        );
    }
    if !mmu.contains("start == 0 && size == 0 || size == usize::MAX") {
        errors.push(
            "AArch64 broadcast TLBI must consume the generic full-range sentinel before aligned range validation"
                .into(),
        );
    }
    if !(mmu.contains("address < DIRECT_MAP_SIZE")
        && mmu.contains(
            "AArch64 direct-map physical address exceeds the current 120 GiB TTBR1 window",
        )
        && mmu.contains("checked_add(address)"))
    {
        errors.push(
            "AArch64 direct-map conversion must reject rather than alias oversized PA".into(),
        );
    }
    if !mmu.contains("USER_STACK_TOP: usize = SIGNAL_TRAMPOLINE_ADDRESS - PAGE_SIZE") {
        errors.push(
            "AArch64 user stack must retain a guard below signal/trap trampoline pages".into(),
        );
    }
    if !(mmu.contains("const DIRECT_MAP_SIZE: usize = 120usize << 30")
        && mmu.contains("KERNEL_STACK_REGION_START: usize = DIRECT_MAP_BASE + DIRECT_MAP_SIZE")
        && mmu.contains("KERNEL_STACK_REGION_TOP: usize = usize::MAX & !(PAGE_SIZE - 1)")
        && riscv_mmu.contains("KERNEL_STACK_REGION_START: usize = 0")
        && riscv_mmu.contains("KERNEL_STACK_REGION_TOP: usize = TRAP_CONTEXT_ADDRESS")
        && kernel_stack.contains("crate::arch::mmu::KERNEL_STACK_REGION_START")
        && kernel_stack.contains("crate::arch::mmu::KERNEL_STACK_REGION_TOP")
        && !kernel_stack.contains("super::TRAP_CONTEXT - app_id"))
    {
        errors.push(
            "AArch64 kernel stacks must occupy the canonical TTBR1 window above the bounded direct map"
                .into(),
        );
    }
    if !(aarch64_context.contains("KERNEL_STACK_CONTEXT_RESERVE: usize = super::mmu::PAGE_SIZE")
        && aarch64_context.contains(
            "KERNEL_STACK_CONTEXT_OFFSET + size_of::<UserContext>() <= KERNEL_STACK_CONTEXT_RESERVE",
        )
        && aarch64_context.contains("mapped_top.checked_sub(KERNEL_STACK_CONTEXT_RESERVE)?")
        && aarch64_context.contains("KERNEL_STACK_CONTEXT_OFFSET: usize = 16")
        && aarch64_context.contains("Some(reserved + KERNEL_STACK_CONTEXT_OFFSET)")
        && aarch64_context.contains(
            "(super::mmu::KERNEL_STACK_REGION_START..super::mmu::KERNEL_STACK_REGION_TOP)",
        )
        && riscv64_context.contains("KERNEL_STACK_CONTEXT_RESERVE: usize = 0")
        && riscv64_context
            .contains("kernel_stack_user_context(_mapped_top: usize) -> Option<usize>")
        && riscv64_context.contains("is_kernel_stack_user_context(_address: usize) -> bool")
        && riscv64_context.matches("None").count() >= 1
        && riscv64_context.matches("false").count() >= 1
        && kernel_stack
            .contains("top.checked_sub(crate::arch::context::KERNEL_STACK_CONTEXT_RESERVE)")
        && kernel_stack.contains("pub(crate) fn user_context_address(&self) -> Option<usize>")
        && kernel_stack.contains("crate::arch::context::kernel_stack_user_context(mapped_top)")
        && task_trap_context
            .contains("crate::arch::context::is_kernel_stack_user_context(address)")
        && task_trap_context
            .contains("crate::arch::context::is_kernel_stack_user_context(owner.address())")
        && task_model.contains("kernel_stack.user_context_address().unwrap_or(TRAP_CONTEXT)")
        && task_model.contains("match kernel_stack.user_context_address()")
        && task_model.contains("allocate_thread_trap_context(tid)?")
        && task_model
            .matches("!crate::arch::context::is_kernel_stack_user_context(user_cx_va)")
            .count()
            == 2
        && task_model
            .matches("memory_retirement_wait: Mutex::new(memory_retirement_wait)")
            .count()
            == 2
        && process_clone.contains("if let Some(address) = kernel_stack.user_context_address()")
        && process_clone.contains("allocate_thread_trap_context(tid)?")
        && process_clone
            .contains("!crate::arch::context::is_kernel_stack_user_context(user_cx_va)")
        && process_clone
            .contains("memory_retirement_wait: Mutex::new(memory_retirement_wait)")
        && process_exec
            .contains("!crate::arch::context::is_kernel_stack_user_context(old_trap_context)"))
    {
        errors.push(
            "AArch64 KernelStack must own context storage without an AddressSpace-retirement waiter while RISC-V keeps the supervisor-VMA backend"
                .into(),
        );
    }
    if !(start.contains("ubfx    x11, x11, #4, #4")
        && start.contains("lsl     x11, x11, #36")
        && startup.contains("u64::from(uses_sixteen_bit_asids) << 36"))
    {
        errors.push(
            "AArch64 boot and local TCR_EL1 must select 16-bit ASIDs only when ASIDBits is 16"
                .into(),
        );
    }
    if !(instruction_cache.contains("dc cvau, {address}")
        && instruction_cache.contains("ic ivau, {address}")
        && instruction_cache
            .matches("physical_to_virtual(physical)")
            .count()
            == 2
        && shootdown.contains("instruction_first_physical_page")
        && shootdown.contains("arch::instruction::publish_range(start, size)"))
    {
        errors.push(
            "AArch64 executable publication must carry physical ranges into the IDC/DIC fallback"
                .into(),
        );
    }
    if !(aarch64_io.contains("\"ldrb {value:w}, [{address}]\"")
        && aarch64_io.contains("\"strb {value:w}, [{address}]\"")
        && aarch64_io.contains("\"ldr {value:w}, [{address}]\"")
        && aarch64_io.contains("\"str {value:w}, [{address}]\"")
        && aarch64_io.contains("\"ldr {value}, [{address}]\"")
        && aarch64_io.contains("\"str {value}, [{address}]\"")
        && mmio_bus.contains("crate::arch::read_mmio_u8(address)")
        && mmio_bus.contains("crate::arch::write_mmio_u8(address, value)")
        && mmio_bus.contains("crate::arch::read_mmio_u32(address)")
        && mmio_bus.contains("crate::arch::write_mmio_u32(address, value)")
        && !mmio_bus.contains("read_volatile")
        && !mmio_bus.contains("write_volatile")
        && gicv3.contains("crate::arch::read_mmio_u32(address)")
        && gicv3.contains("crate::arch::write_mmio_u64(address, value)"))
    {
        errors.push(
            "AArch64 MMIO must use the arch-owned exact base-register loads/stores required by QEMU HVF"
                .into(),
        );
    }
    if !(heap
        .matches("arch::mmu::physical_to_virtual(physical)")
        .count()
        == 2
        && heap.matches("arch::mmu::virtual_to_physical(").count() == 2
        && virtio_queue.contains("base_pa.as_mut_ptr::<u8>()")
        && gicv3
            .matches("crate::arch::mmu::physical_to_virtual(")
            .count()
            == 2)
    {
        errors.push(
            "frame-backed heap and VirtIO rings must use the architecture direct map instead of raw physical pointers"
                .into(),
        );
    }
}

fn bootstrap_wfi_has_exact_irq_resume(trap: &str, interrupt: &str) -> bool {
    let Some(wait) = trap.find("__wait_with_local_irq_masked:") else {
        return false;
    };
    let wrapper = &trap[wait..];
    let enable = wrapper.find("msr daifclr, #2").map(|offset| wait + offset);
    let sleep = wrapper
        .find("__local_irq_wait_wfi:\n    wfi")
        .map(|offset| wait + offset);
    let resume = wrapper
        .find("__local_irq_wait_wfi_resume:")
        .map(|offset| wait + offset);
    let disable = wrapper.find("msr daifset, #2").map(|offset| wait + offset);
    Some(wait)
        .zip(enable)
        .zip(sleep)
        .zip(resume)
        .zip(disable)
        .is_some_and(|((((wait, enable), sleep), resume), disable)| {
            wait < enable && enable < sleep && sleep < resume && resume < disable
        })
        && trap.contains("adr x9, __local_irq_wait_wfi")
        && trap.contains("adr x8, __local_irq_wait_wfi_resume")
        && trap.contains("msr elr_el1, x8")
        && interrupt.contains("fn __wait_with_local_irq_masked();")
        && interrupt.contains("__wait_with_local_irq_masked()")
}

fn read(root: &Path, relative: &str, errors: &mut Vec<String>) -> Result<String, ()> {
    fs::read_to_string(root.join(relative)).map_err(|error| {
        errors.push(format!("{relative}: {error}"));
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn repository_bootstrap_wfi_cannot_reenter_after_ack() {
        let root = super::super::repository_root();
        let trap = std::fs::read_to_string(root.join(super::TRAP)).expect("trap assembly");
        let interrupt =
            std::fs::read_to_string(root.join(super::INTERRUPT)).expect("interrupt source");
        assert!(super::bootstrap_wfi_has_exact_irq_resume(&trap, &interrupt));
    }
}
