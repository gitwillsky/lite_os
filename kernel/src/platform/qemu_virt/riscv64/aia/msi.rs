//! 消息组的逻辑 vector 租约与两阶段 CPU/EIID 迁移；目标地址不假定组内固定。
use super::{Cold, Controller, Kind, MESSAGE_BASE, Route, Target, controller};
use crate::execution::{
    cpu::{self, CpuId},
    sync::{TaskMutexGuard, TaskMutexWaitPreparation},
};
use alloc::vec::Vec;
use fallible_tree::FallibleMap;
use native_abi::{ERR_ALREADY_BOUND, ERR_INVALID_ARGS, ERR_NO_MEMORY, ERR_NO_RESOURCES, Status};

/// 同一设备独占的稳定逻辑 vector；物理 EIID 在子 Interrupt 的实际创建/绑定阶段准备。
pub(crate) struct MessageInterruptGroup {
    device: u32,
    vectors: Vec<u32>,
    retirement: TaskMutexWaitPreparation,
}
impl MessageInterruptGroup {
    /// 分配逻辑槽；不预留未使用 CPU 的物理向量，不发布可发消息的源。
    /// # Errors
    /// count 非法、同设备已有组、内存或逻辑槽不足。
    pub(crate) fn allocate(
        device: u32,
        count: u32,
        _allocator: crate::platform::MessageInterruptAllocator,
    ) -> Result<Self, Status> {
        let controller = controller();
        let capacity = (controller.identities as usize - 1) * controller.files.len();
        if count == 0 {
            return Err(ERR_INVALID_ARGS);
        }
        if count as usize > capacity {
            return Err(ERR_NO_RESOURCES);
        }
        let retirement = TaskMutexWaitPreparation::prepare().map_err(|_| ERR_NO_MEMORY)?;
        let mut cold = controller.cold.lock().map_err(|_| ERR_NO_MEMORY)?;
        if cold.devices.contains_key(&device) {
            return Err(ERR_ALREADY_BOUND);
        }
        let prepared_device = FallibleMap::try_prepare(device, ()).map_err(|_| ERR_NO_MEMORY)?;
        let mut vectors = Vec::new();
        let mut prepared = Vec::new();
        vectors
            .try_reserve_exact(count as usize)
            .map_err(|_| ERR_NO_MEMORY)?;
        prepared
            .try_reserve_exact(count as usize)
            .map_err(|_| ERR_NO_MEMORY)?;
        for bit in 0..capacity {
            if cold.logical[bit / 64] & (1 << (bit % 64)) != 0 {
                continue;
            }
            let vector = MESSAGE_BASE + bit as u32;
            vectors.push(vector);
            prepared.push(
                FallibleMap::try_prepare(
                    vector,
                    Route {
                        target: None,
                        handler: None,
                        kind: Kind::Message,
                    },
                )
                .map_err(|_| ERR_NO_MEMORY)?,
            );
            if vectors.len() == count as usize {
                break;
            }
        }
        if vectors.len() != count as usize {
            return Err(ERR_NO_RESOURCES);
        }
        for vector in &vectors {
            let bit = (vector - MESSAGE_BASE) as usize;
            cold.logical[bit / 64] |= 1 << (bit % 64);
        }
        cold.devices.commit_vacant(prepared_device);
        let mut routes = controller.routes.lock();
        for node in prepared {
            routes.vectors.commit_vacant(node);
        }
        Ok(Self {
            device,
            vectors,
            retirement,
        })
    }
    pub(crate) fn count(&self) -> u32 {
        self.vectors.len() as u32
    }
    pub(crate) fn vector(&self, index: u32) -> Option<u32> {
        self.vectors.get(index as usize).copied()
    }
    /// 预留目标 CPU 的 EIID；错误不改旧路由，丢弃令牌归还尚未发布的新槽。
    pub(crate) fn prepare_affinity(
        &self,
        index: u32,
        cpu: CpuId,
    ) -> Result<MessageInterruptRoute<'_>, Status> {
        if !cpu::online().contains(cpu) {
            return Err(ERR_INVALID_ARGS);
        }
        let vector = self.vector(index).ok_or(ERR_INVALID_ARGS)?;
        let controller = controller();
        let cold = controller.cold.lock().map_err(|_| ERR_NO_MEMORY)?;
        let mut routes = controller.routes.lock();
        let previous = routes
            .vectors
            .get(&vector)
            .expect("owned MSI vector")
            .target;
        let next = if let Some(previous) = previous.filter(|previous| previous.cpu == cpu) {
            previous
        } else {
            routes.reserve(vector, cpu)?
        };
        Ok(MessageInterruptRoute {
            controller,
            cold,
            vector,
            previous,
            next,
            committed: false,
        })
    }
}
impl Drop for MessageInterruptGroup {
    fn drop(&mut self) {
        let controller = controller();
        let mut cold = controller.cold.lock_prepared(&mut self.retirement);
        let mut routes = controller.routes.lock();
        for vector in &self.vectors {
            let route = routes
                .vectors
                .remove(vector)
                .expect("retiring MSI group vector");
            assert!(
                route.handler.is_none(),
                "child Interrupt must unbind before group retirement"
            );
            if let Some(target) = route.target {
                // 仅构造失败、从未绑定的 masked 源可留下 target；EIE/EIP 已在该次 commit 确认关闭。
                assert_eq!(
                    routes.locals[target.cpu.index()].slots[target.id as usize],
                    *vector
                );
                routes.locals[target.cpu.index()].slots[target.id as usize] = 0;
            }
            let bit = (vector - MESSAGE_BASE) as usize;
            cold.logical[bit / 64] &= !(1 << (bit % 64));
        }
        cold.devices.remove(&self.device).expect("MSI device lease");
    }
}

/// 持有平台冷锁直到源恢复完成；取消只归还新预留，提交后不能以可恢复错误伪装回滚。
pub(crate) struct MessageInterruptRoute<'a> {
    controller: &'a Controller,
    cold: TaskMutexGuard<'a, Cold>,
    vector: u32,
    previous: Option<Target>,
    next: Target,
    // OWNER: 令牌独占发布状态；提交后 Drop 不得归还已经供硬件使用的 EIID。
    committed: bool,
}
impl MessageInterruptRoute<'_> {
    pub(crate) fn message(&self) -> (u64, u32) {
        (
            self.controller.files[self.next.cpu.index()].address,
            self.next.id,
        )
    }
    /// 源表项已 mask/readback 并写入新消息值；ack 回调只处理上层本地请求，不等待或分配。
    pub(crate) fn commit(&mut self, acknowledge_local: impl Fn()) {
        assert!(!self.committed, "MSI route commits once");
        self.controller
            .move_route(&mut self.cold, self.vector, self.next, acknowledge_local);
        self.committed = true;
    }
}
impl Drop for MessageInterruptRoute<'_> {
    fn drop(&mut self) {
        if !self.committed && self.previous != Some(self.next) {
            let mut routes = self.controller.routes.lock();
            let slot = &mut routes.locals[self.next.cpu.index()].slots[self.next.id as usize];
            assert_eq!(
                *slot, self.vector,
                "unpublished EIID reservation is exclusive"
            );
            *slot = 0;
        }
    }
}
