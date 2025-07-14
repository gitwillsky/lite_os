use alloc::{sync::Arc, vec::Vec, collections::BTreeMap};
use crate::{
    thread::{ThreadControlBlock, ThreadId, ThreadStatus, ThreadStack, alloc_thread_id},
    task::TaskControlBlock,
    memory::{
        frame_allocator::alloc,
        config::{KERNEL_STACK_SIZE, USER_STACK_SIZE, PAGE_SIZE},
        address::{VirtualAddress, PhysicalPageNumber, VirtualPageNumber},
        mm::{MemorySet, MapArea, MapType, MapPermission},
        page_table::{PageTable, PTEFlags},
    },
    trap::TrapContext,
    timer::get_time_us,
};

/// 线程栈分配器
#[derive(Debug)]
pub struct ThreadStackAllocator {
    /// 下一个可分配的用户栈基地址
    next_user_stack_base: VirtualAddress,
    /// 用户栈大小
    stack_size: usize,
    /// 已分配的栈列表
    allocated_stacks: Vec<(VirtualAddress, usize)>,
}

impl ThreadStackAllocator {
    pub fn new(initial_base: VirtualAddress, stack_size: usize) -> Self {
        Self {
            next_user_stack_base: initial_base,
            stack_size,
            allocated_stacks: Vec::new(),
        }
    }

    /// 分配用户栈
    pub fn alloc_user_stack(&mut self, memory_set: &mut MemorySet) -> Result<ThreadStack, &'static str> {
        let stack_base = self.next_user_stack_base;
        let stack_top = VirtualAddress::from(stack_base.as_usize() + self.stack_size);
        
        // 更新下一个栈的基地址（留出一个页面作为保护）
        self.next_user_stack_base = VirtualAddress::from(stack_top.as_usize() + PAGE_SIZE);
        
        // 在虚拟地址空间中映射栈区域
        let start_vpn = VirtualPageNumber::from(stack_base);
        let end_vpn = VirtualPageNumber::from(stack_top);
        
        let map_area = MapArea::new(
            start_vpn,
            end_vpn,
            MapType::Framed,
            MapPermission::R | MapPermission::W | MapPermission::U,
        );
        
        memory_set.insert_framed_area(map_area)
            .map_err(|_| "Failed to map user stack")?;
        
        // 记录已分配的栈
        self.allocated_stacks.push((stack_base, self.stack_size));
        
        Ok(ThreadStack::new(stack_base, self.stack_size))
    }

    /// 释放用户栈
    pub fn dealloc_user_stack(
        &mut self, 
        stack: &ThreadStack, 
        memory_set: &mut MemorySet
    ) -> Result<(), &'static str> {
        // 从内存集合中移除栈区域
        let start_vpn = VirtualPageNumber::from(stack.start_va);
        let end_vpn = VirtualPageNumber::from(stack.end_va);
        
        memory_set.remove_area_with_start_vpn(start_vpn)
            .map_err(|_| "Failed to unmap user stack")?;
        
        // 从已分配列表中移除
        self.allocated_stacks.retain(|(base, _)| *base != stack.start_va);
        
        Ok(())
    }
}

/// 线程管理器
#[derive(Debug)]
pub struct ThreadManager {
    /// 线程表，映射线程ID到线程控制块
    threads: BTreeMap<ThreadId, Arc<ThreadControlBlock>>,
    /// 就绪线程队列
    ready_queue: Vec<ThreadId>,
    /// 阻塞线程列表
    blocked_threads: Vec<ThreadId>,
    /// 当前运行的线程ID
    current_thread: Option<ThreadId>,
    /// 主线程ID
    main_thread_id: ThreadId,
    /// 所属进程
    parent_process: Arc<TaskControlBlock>,
    /// 栈分配器
    stack_allocator: ThreadStackAllocator,
    /// 线程统计信息
    thread_count: usize,
    max_threads: usize,
}

impl ThreadManager {
    /// 创建新的线程管理器
    pub fn new(parent_process: Arc<TaskControlBlock>) -> Self {
        let main_thread_id = alloc_thread_id();
        
        // 设置用户栈分配的起始地址（在用户地址空间的高端）
        let initial_stack_base = VirtualAddress::from(0x80000000usize);
        let stack_allocator = ThreadStackAllocator::new(initial_stack_base, USER_STACK_SIZE);
        
        Self {
            threads: BTreeMap::new(),
            ready_queue: Vec::new(),
            blocked_threads: Vec::new(),
            current_thread: Some(main_thread_id),
            main_thread_id,
            parent_process,
            stack_allocator,
            thread_count: 1, // 主线程
            max_threads: 1024, // 最大线程数限制
        }
    }

    /// 创建新线程
    pub fn create_thread(
        &mut self,
        entry_point: usize,
        stack_size: usize,
        arg: usize,
        joinable: bool,
    ) -> Result<ThreadId, &'static str> {
        if self.thread_count >= self.max_threads {
            return Err("Maximum thread limit reached");
        }

        let thread_id = alloc_thread_id();
        
        // 分配内核栈
        let kernel_stack = self.alloc_kernel_stack()?;
        
        // 分配用户栈
        let mut process_inner = self.parent_process.inner_exclusive_access();
        let user_stack = self.stack_allocator.alloc_user_stack(&mut process_inner.memory_set)
            .map_err(|_| "Failed to allocate user stack")?;
        
        // 分配陷入上下文页面
        let trap_cx_ppn = self.alloc_trap_context_page(&mut process_inner.memory_set)?;
        
        drop(process_inner);
        
        // 创建线程控制块
        let thread = Arc::new(ThreadControlBlock::new(
            thread_id,
            Arc::downgrade(&self.parent_process),
            entry_point,
            user_stack,
            kernel_stack,
            KERNEL_STACK_SIZE,
            trap_cx_ppn,
            arg,
            joinable,
        ));
        
        // 加入线程表和就绪队列
        self.threads.insert(thread_id, thread);
        self.ready_queue.push(thread_id);
        self.thread_count += 1;
        
        Ok(thread_id)
    }

    /// 退出当前线程
    pub fn exit_thread(&mut self, exit_code: i32) {
        if let Some(current_id) = self.current_thread {
            if let Some(thread) = self.threads.get(&current_id) {
                thread.exit(exit_code);
                
                // 如果是主线程退出，则终止整个进程
                if current_id == self.main_thread_id {
                    self.terminate_process(exit_code);
                    return;
                }
                
                // 从就绪队列中移除
                self.ready_queue.retain(|&id| id != current_id);
                self.blocked_threads.retain(|&id| id != current_id);
                
                // 立即清理线程资源
                self.cleanup_thread_immediate(current_id);
                self.current_thread = None;
                self.thread_count -= 1;
                
                // 如果没有更多线程，终止进程
                if self.thread_count == 0 {
                    self.terminate_process(0);
                    return;
                }
            }
        }
        
        // 调度下一个线程
        self.schedule_next();
    }

    /// 等待线程结束
    pub fn join_thread(&mut self, target_thread_id: ThreadId) -> Result<i32, &'static str> {
        // 检查目标线程是否存在
        if !self.threads.contains_key(&target_thread_id) {
            return Err("Thread not found");
        }
        
        // 检查目标线程是否可以被join
        if let Some(target_thread) = self.threads.get(&target_thread_id) {
            if !target_thread.is_joinable() {
                return Err("Thread not joinable");
            }
            
            // 如果线程已经退出，直接返回退出码
            if target_thread.get_status() == ThreadStatus::Exited {
                let exit_code = target_thread.get_exit_code();
                // 清理已退出的线程
                self.cleanup_thread(target_thread_id);
                return Ok(exit_code);
            }
            
            // 阻塞当前线程，等待目标线程退出
            if let Some(current_id) = self.current_thread {
                target_thread.add_waiting_thread(current_id);
                
                // 阻塞当前线程
                if let Some(current_thread) = self.threads.get(&current_id) {
                    current_thread.set_status(ThreadStatus::Blocked);
                    self.ready_queue.retain(|&id| id != current_id);
                    self.blocked_threads.push(current_id);
                }
                
                // 调度下一个线程
                self.current_thread = None;
                self.schedule_next();
            }
        }
        
        Err("Join failed")
    }

    /// 唤醒线程
    pub fn wakeup_thread(&mut self, thread_id: ThreadId) {
        if let Some(thread) = self.threads.get(&thread_id) {
            if thread.get_status() == ThreadStatus::Blocked {
                thread.set_status(ThreadStatus::Ready);
                self.blocked_threads.retain(|&id| id != thread_id);
                self.ready_queue.push(thread_id);
            }
        }
    }

    /// 调度下一个线程
    pub fn schedule_next(&mut self) {
        let old_current = self.current_thread;
        
        // 简单的轮转调度 + 优先级调度
        if let Some(next_thread_id) = self.ready_queue.pop() {
            if let Some(thread) = self.threads.get(&next_thread_id) {
                thread.set_status(ThreadStatus::Running);
                self.current_thread = Some(next_thread_id);
                
                // 只有在切换到不同线程时才执行上下文切换
                if old_current != Some(next_thread_id) {
                    self.context_switch_to(next_thread_id);
                } else {
                    // 如果调度到同一个线程，只需要准备和完成切换
                    thread.prepare_context_switch();
                    thread.finish_context_switch();
                }
            }
        } else {
            // 没有可运行的线程
            self.current_thread = None;
        }
    }

    /// 执行上下文切换
    fn context_switch_to(&mut self, target_thread_id: ThreadId) {
        if let Some(current_id) = self.current_thread {
            if let Some(current_thread) = self.threads.get(&current_id) {
                if let Some(target_thread) = self.threads.get(&target_thread_id) {
                    let mut current_inner = current_thread.inner_exclusive_access();
                    let target_inner = target_thread.inner_exclusive_access();
                    
                    // 准备上下文切换
                    current_thread.prepare_context_switch();
                    target_thread.prepare_context_switch();
                    
                    // 获取上下文指针
                    let current_cx_ptr = current_inner.get_context_ptr();
                    let target_cx_ptr = &target_inner.context as *const crate::task::TaskContext;
                    
                    drop(current_inner);
                    drop(target_inner);
                    
                    // 执行线程级别的上下文切换
                    unsafe {
                        crate::task::schedule_thread(current_cx_ptr, target_cx_ptr);
                    }
                    
                    // 完成上下文切换
                    current_thread.finish_context_switch();
                    target_thread.finish_context_switch();
                }
            }
        } else {
            // 如果没有当前线程，直接设置目标线程为运行状态
            if let Some(target_thread) = self.threads.get(&target_thread_id) {
                target_thread.prepare_context_switch();
                target_thread.finish_context_switch();
            }
        }
    }

    /// 获取当前线程
    pub fn get_current_thread(&self) -> Option<Arc<ThreadControlBlock>> {
        if let Some(current_id) = self.current_thread {
            self.threads.get(&current_id).cloned()
        } else {
            None
        }
    }

    /// 线程让步
    pub fn yield_thread(&mut self) {
        if let Some(current_id) = self.current_thread {
            if let Some(thread) = self.threads.get(&current_id) {
                thread.set_status(ThreadStatus::Ready);
                self.ready_queue.push(current_id);
            }
            
            // 保存当前线程ID用于上下文切换
            let old_current = self.current_thread;
            self.current_thread = None;
            
            // 寻找下一个可运行的线程
            if let Some(next_thread_id) = self.ready_queue.pop() {
                if let Some(next_thread) = self.threads.get(&next_thread_id) {
                    next_thread.set_status(ThreadStatus::Running);
                    self.current_thread = Some(next_thread_id);
                    
                    // 如果切换到不同的线程，执行上下文切换
                    if old_current != Some(next_thread_id) {
                        self.context_switch_to(next_thread_id);
                    }
                }
            }
        }
    }

    /// 终止进程（所有线程）
    fn terminate_process(&mut self, exit_code: i32) {
        info!("Terminating process with {} threads", self.thread_count);
        
        // 设置所有线程为退出状态
        for thread in self.threads.values() {
            thread.exit(exit_code);
        }
        
        // 清理所有线程
        let thread_ids: Vec<ThreadId> = self.threads.keys().cloned().collect();
        for thread_id in thread_ids {
            self.cleanup_thread_immediate(thread_id);
        }
        
        self.ready_queue.clear();
        self.blocked_threads.clear();
        self.current_thread = None;
        self.thread_count = 0;
        
        info!("Process terminated successfully");
    }

    /// 分配内核栈
    fn alloc_kernel_stack(&self) -> Result<usize, &'static str> {
        if let Some(frame) = alloc() {
            let kernel_stack_bottom: crate::memory::address::PhysicalAddress = frame.ppn.into();
            Ok(kernel_stack_bottom.as_usize())
        } else {
            Err("Failed to allocate kernel stack")
        }
    }

    /// 释放内核栈
    fn dealloc_kernel_stack(&self, _kernel_stack_base: usize) {
        // 这里应该释放内核栈页面
        // 由于当前的frame_allocator没有提供按地址释放的接口
        // 这里暂时留空，实际实现需要扩展frame_allocator
    }

    /// 分配陷入上下文页面
    fn alloc_trap_context_page(&self, memory_set: &mut MemorySet) -> Result<PhysicalPageNumber, &'static str> {
        if let Some(frame) = alloc() {
            // 找一个未使用的虚拟地址来映射陷入上下文
            // 这里简化处理，实际应该有更好的地址管理
            let trap_cx_va = VirtualAddress::from(0x10000000usize + self.thread_count * PAGE_SIZE);
            let trap_cx_vpn = VirtualPageNumber::from(trap_cx_va);
            
            // 在页表中建立映射
            memory_set.page_table.map(
                trap_cx_vpn,
                frame.ppn,
                PTEFlags::R | PTEFlags::W
            );
            
            Ok(frame.ppn)
        } else {
            Err("Failed to allocate trap context page")
        }
    }

    /// 释放陷入上下文页面
    fn dealloc_trap_context_page(&self, _ppn: PhysicalPageNumber, _memory_set: &mut MemorySet) {
        // 这里应该取消页表映射并释放物理页面
        // 暂时留空
    }

    /// 获取线程数量
    pub fn thread_count(&self) -> usize {
        self.thread_count
    }

    /// 检查是否有活跃线程
    pub fn has_active_threads(&self) -> bool {
        self.thread_count > 0
    }

    /// 获取就绪线程数量
    pub fn ready_thread_count(&self) -> usize {
        self.ready_queue.len()
    }

    /// 获取阻塞线程数量
    pub fn blocked_thread_count(&self) -> usize {
        self.blocked_threads.len()
    }

    /// 设置最大线程数
    pub fn set_max_threads(&mut self, max: usize) {
        self.max_threads = max;
    }

    /// 根据线程ID查找线程
    pub fn find_thread(&self, thread_id: ThreadId) -> Option<Arc<ThreadControlBlock>> {
        self.threads.get(&thread_id).cloned()
    }

    /// 获取线程统计信息
    pub fn get_thread_stats(&self) -> (usize, usize, usize) {
        (self.thread_count, self.ready_queue.len(), self.blocked_threads.len())
    }
    
    /// 处理线程异常（线程崩溃等）
    pub fn handle_thread_exception(&mut self, thread_id: ThreadId, exception_code: i32) {
        warn!("Thread {} encountered exception: {}", thread_id.0, exception_code);
        
        if let Some(thread) = self.threads.get(&thread_id) {
            // 如果是主线程出现异常，终止整个进程
            if thread_id == self.main_thread_id {
                error!("Main thread exception, terminating process");
                self.terminate_process(exception_code);
                return;
            }
            
            // 普通线程异常，只终止该线程
            thread.exit(exception_code);
            
            // 从队列中移除
            self.ready_queue.retain(|&id| id != thread_id);
            self.blocked_threads.retain(|&id| id != thread_id);
            
            // 清理资源
            self.cleanup_thread_immediate(thread_id);
            self.thread_count -= 1;
            
            // 如果当前线程就是异常线程，需要调度下一个
            if self.current_thread == Some(thread_id) {
                self.current_thread = None;
                self.schedule_next();
            }
        }
    }
    
    /// 批量清理已退出的线程
    pub fn cleanup_exited_threads(&mut self) {
        let mut threads_to_cleanup = Vec::new();
        
        // 找到所有已退出的线程
        for (&thread_id, thread) in &self.threads {
            if thread.get_status() == ThreadStatus::Exited {
                threads_to_cleanup.push(thread_id);
            }
        }
        
        // 清理这些线程
        for thread_id in threads_to_cleanup {
            if thread_id != self.main_thread_id { // 不清理主线程
                self.cleanup_thread_immediate(thread_id);
                self.thread_count -= 1;
            }
        }
    }
    
    /// 检查线程管理器状态是否健康
    pub fn health_check(&self) -> bool {
        // 检查是否有活跃线程
        if self.thread_count == 0 {
            return false;
        }
        
        // 检查主线程是否存在
        if !self.threads.contains_key(&self.main_thread_id) {
            return false;
        }
        
        // 检查状态一致性
        let actual_ready = self.threads.values()
            .filter(|t| t.get_status() == ThreadStatus::Ready)
            .count();
        let actual_blocked = self.threads.values()
            .filter(|t| t.get_status() == ThreadStatus::Blocked)
            .count();
        
        // 容忍一些不一致，但不应该有太大差异
        let ready_diff = (self.ready_queue.len() as i32 - actual_ready as i32).abs();
        let blocked_diff = (self.blocked_threads.len() as i32 - actual_blocked as i32).abs();
        
        ready_diff <= 2 && blocked_diff <= 2
    }
    
    /// 立即清理线程资源
    fn cleanup_thread_immediate(&mut self, thread_id: ThreadId) {
        if let Some(thread) = self.threads.remove(&thread_id) {
            let thread_inner = thread.inner_exclusive_access();
            
            // 释放用户栈
            let mut process_inner = self.parent_process.inner_exclusive_access();
            if let Err(e) = self.stack_allocator.dealloc_user_stack(
                &thread_inner.user_stack, 
                &mut process_inner.memory_set
            ) {
                warn!("Failed to deallocate user stack for thread {}: {}", thread_id.0, e);
            }
            
            // 释放陷入上下文页面
            self.dealloc_trap_context_page(thread_inner.trap_cx_ppn, &mut process_inner.memory_set);
            
            // 释放内核栈
            self.dealloc_kernel_stack(thread_inner.kernel_stack_base);
            
            drop(process_inner);
            drop(thread_inner);
            
            info!("Thread {} resources cleaned up", thread_id.0);
        }
    }
}

/// 全局线程管理器接口函数

/// 创建线程
pub fn create_thread(
    entry_point: usize,
    stack_size: usize,
    arg: usize,
    joinable: bool,
) -> Result<ThreadId, &'static str> {
    if let Some(current_task) = crate::task::current_task() {
        let mut task_inner = current_task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
            thread_manager.create_thread(entry_point, stack_size, arg, joinable)
        } else {
            Err("Thread manager not initialized")
        }
    } else {
        Err("No current task")
    }
}

/// 退出线程
pub fn exit_thread(exit_code: i32) {
    if let Some(current_task) = crate::task::current_task() {
        let mut task_inner = current_task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
            thread_manager.exit_thread(exit_code);
        }
    }
}

/// 等待线程
pub fn join_thread(thread_id: ThreadId) -> Result<i32, &'static str> {
    if let Some(current_task) = crate::task::current_task() {
        let mut task_inner = current_task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
            thread_manager.join_thread(thread_id)
        } else {
            Err("Thread manager not initialized")
        }
    } else {
        Err("No current task")
    }
}