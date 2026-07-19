use super::{
    KERNEL_SPACE, KERNEL_STACK_SIZE, MapPermission, MemoryError, PAGE_SIZE, address::VirtualAddress,
};
use crate::id::IdAllocator;
use spin::Mutex;

#[derive(Debug)]
pub(crate) struct KernelStack {
    handle: KernelStackHandle,
}

impl KernelStack {
    /// @description 分配带 guard page 的 kernel stack，供可失败的 process 创建事务使用。
    ///
    /// @return 成功返回唯一 stack handle；frame OOM 时回滚映射并归还 handle。
    pub(crate) fn try_new() -> Result<Self, MemoryError> {
        let handle = KernelStackHandle(
            KERNEL_STACK_HANDLE_ALLOCATOR
                .lock()
                .alloc()
                .map_err(|_| MemoryError::OutOfMemory)?,
        );
        let (bottom, top) = kernel_stack_position(handle.0);

        // 在栈底预留 1 页守护页，防止向下越界破坏相邻对象导致不可预期行为；
        // 溢出将立刻触发缺页异常，便于定位问题
        let mapped_bottom = bottom + PAGE_SIZE;

        KERNEL_SPACE.wait().lock().insert_framed_area(
            mapped_bottom.into(),
            top.into(),
            MapPermission::R | MapPermission::W,
        )?;

        Ok(Self { handle })
    }

    pub(crate) fn get_top(&self) -> usize {
        let (_, top) = kernel_stack_position(self.handle.0);
        top.checked_sub(crate::arch::context::KERNEL_STACK_CONTEXT_RESERVE)
            .expect("kernel stack context reserve exceeds mapping")
    }

    /// @description 返回 architecture 选择的 kernel-stack-owned UserContext 地址。
    /// @return AArch64 为保留顶页 metadata 后的 context；RISC-V 为 None。
    pub(crate) fn user_context_address(&self) -> Option<usize> {
        let (_, mapped_top) = kernel_stack_position(self.handle.0);
        match crate::arch::context::USER_CONTEXT_PLACEMENT {
            crate::arch::UserContextPlacement::KernelStack { offset } => Some(
                mapped_top
                    .checked_sub(crate::arch::context::KERNEL_STACK_CONTEXT_RESERVE)
                    .and_then(|reserved| reserved.checked_add(offset))
                    .expect("kernel-stack user-context placement exceeds mapping"),
            ),
            crate::arch::UserContextPlacement::AddressSpace => None,
        }
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let (bottom, _) = kernel_stack_position(self.handle.0);
        let mapped_bottom = bottom + PAGE_SIZE;
        KERNEL_SPACE
            .wait()
            .lock()
            .remove_area_with_start_vpn(VirtualAddress::from(mapped_bottom).into());
    }
}

/// 获取应用内核栈的地址范围，返回 (bottom, top)
fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    // architecture façade owns the TTBR-visible stack window. AArch64 uses the canonical TTBR1
    // region above its bounded direct map; RISC-V preserves the Sv39 layout below trap context.
    let stride = KERNEL_STACK_SIZE
        .checked_add(PAGE_SIZE)
        .expect("kernel stack stride overflow");
    let offset = app_id
        .checked_mul(stride)
        .expect("kernel stack handle exceeds virtual window");
    let top = crate::arch::mmu::KERNEL_STACK_REGION_TOP
        .checked_sub(offset)
        .expect("kernel stack virtual window exhausted");
    let bottom = top
        .checked_sub(KERNEL_STACK_SIZE)
        .expect("kernel stack bottom underflow");
    bottom
        .checked_sub(crate::arch::mmu::KERNEL_STACK_REGION_START)
        .expect("kernel stack virtual window exhausted");
    (bottom, top)
}

#[derive(Debug)]
struct KernelStackHandle(usize);

impl Drop for KernelStackHandle {
    fn drop(&mut self) {
        KERNEL_STACK_HANDLE_ALLOCATOR.lock().dealloc(self.0);
    }
}

// OWNER: kernel-stack module exclusively allocates virtual stack handles.
static KERNEL_STACK_HANDLE_ALLOCATOR: Mutex<IdAllocator> = Mutex::new(IdAllocator::new(1));
