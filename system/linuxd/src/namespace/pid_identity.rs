//! 实际 PID identity 与 graph registration；observer Arc 不拥有 namespace 编号占用。
use crate::namespace::{PidAllocationError, PidKey, PidNamespace, PidReservation};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::mem::MaybeUninit;
use spin::Mutex;

struct Number {
    namespace: Arc<PidNamespace>,
    number: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Prepared,
    Published,
    Released,
}
struct RegistrationState {
    links: usize,
    phase: Phase,
}

/// 编号 ladder 和稳定 key 永不改变；只有 graph registration 计数/phase 可变。
/// TCB、proc 与其他 observer 可持有历史 identity，但不能因此延迟 local number 回收。
pub(crate) struct PidIdentity {
    key: PidKey,
    // leaf→initial 顺序；level 给出常数时间的 viewer index，无另一份投影 cache。
    numbers: Vec<Number>,
    registrations: Mutex<RegistrationState>,
}
impl core::fmt::Debug for PidIdentity {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PidIdentity")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}
impl PidIdentity {
    /// Linux init_struct_pid：所有 CPU 的 boot/idle 执行体共享保留的 initial PID0。
    /// 没有 user association/registration，永不由 cyclic allocator 分配，也不能进入 graph。
    pub(super) fn idle(namespace: Arc<PidNamespace>) -> Result<Arc<Self>, PidAllocationError> {
        assert!(
            namespace.parent().is_none(),
            "idle belongs to initial PID namespace"
        );
        let mut numbers = Vec::new();
        numbers
            .try_reserve_exact(1)
            .map_err(|_| PidAllocationError::OutOfMemory)?;
        numbers.push(Number {
            namespace,
            number: 0,
        });
        Arc::try_new(Self {
            key: PidKey::from_identity(0),
            numbers,
            registrations: Mutex::new(RegistrationState {
                links: 0,
                phase: Phase::Released,
            }),
        })
        .map_err(|_| PidAllocationError::OutOfMemory)
    }
    pub(crate) fn key(&self) -> usize {
        self.key.identity()
    }
    pub(crate) fn namespace(&self) -> Arc<PidNamespace> {
        self.numbers
            .first()
            .expect("execution PID has actual namespace ladder")
            .namespace
            .clone()
    }
    /// 只投影创建时的实际编号；view 不在 ladder 内时返回 0，包括 descendant/unrelated view。
    pub(crate) fn number_in(&self, view: &PidNamespace) -> u32 {
        let leaf = self
            .numbers
            .first()
            .expect("execution PID has actual namespace ladder");
        let Some(index) = leaf.namespace.level().checked_sub(view.level()) else {
            return 0;
        };
        let selected = &self.numbers[usize::from(index)];
        if core::ptr::eq(selected.namespace.as_ref(), view) {
            selected.number
        } else {
            0
        }
    }
    /// proc status 从挂载 view 到实际 leaf 的编号序列；无关 view 返回空序列，不分配。
    pub(crate) fn numbers_in(&self, view: &PidNamespace) -> impl Iterator<Item = u32> + '_ {
        let count = if self.number_in(view) == 0 {
            0
        } else {
            usize::from(self.numbers[0].namespace.level() - view.level()) + 1
        };
        self.numbers[..count]
            .iter()
            .rev()
            .map(|number| number.number)
    }
    pub(crate) fn is_namespace_init(&self) -> bool {
        self.numbers[0].number == 1
    }
    fn add_link(self: &Arc<Self>, slot: PidLinkSlot) -> PidRegistration {
        {
            let mut state = self.registrations.lock();
            assert_ne!(
                state.phase,
                Phase::Released,
                "released identity cannot return to graph"
            );
            state.links = state
                .links
                .checked_add(1)
                .expect("PID registration count overflow");
        }
        slot.fill(self.clone())
    }
    fn release_link(&self) {
        let release = {
            let mut state = self.registrations.lock();
            assert!(state.links != 0, "PID registration released twice");
            state.links -= 1;
            if state.links == 0 {
                let published = state.phase == Phase::Published;
                state.phase = Phase::Released;
                published
            } else {
                false
            }
        };
        if release {
            for number in &self.numbers {
                // initial PID1 association 由 boot 永久拥有；所有其他编号随最后 graph link 释放。
                if self.key != PidKey::INIT {
                    number.namespace.release(number.number, self.key);
                }
            }
        }
    }
}

/// 未发布 identity transaction；reservation storage 在 graph commit 后仍由 caller 锁外释放。
pub(crate) struct PreparedPid {
    identity: Arc<PidIdentity>,
    registration: Option<PidRegistration>,
    reservations: Vec<PidReservation>,
}
impl PreparedPid {
    /// leaf→ancestor 逐级预留不可见编号，任一分配失败归还所有已成功的预留。
    pub(crate) fn try_new(
        key: usize,
        leaf: Arc<PidNamespace>,
        requested: &[i32],
        authority: &super::NamespaceAuthority,
    ) -> Result<Self, PidAllocationError> {
        let key = PidKey::from_identity(key);
        assert_ne!(key, PidKey::INIT, "boot owns initial identity");
        let depth = usize::from(leaf.level()) + 1;
        if requested.len() > depth {
            return Err(PidAllocationError::Invalid);
        }
        let mut reservations = Vec::new();
        let mut numbers = Vec::new();
        reservations
            .try_reserve_exact(depth)
            .map_err(|_| PidAllocationError::OutOfMemory)?;
        numbers
            .try_reserve_exact(depth)
            .map_err(|_| PidAllocationError::OutOfMemory)?;
        let link = PidLinkSlot::try_new()?;
        let mut current = Some(leaf);
        while let Some(namespace) = current {
            let requested = requested.get(numbers.len()).copied();
            if let Some(number) = requested {
                if number < 1 || number as u32 >= namespace.limit() {
                    return Err(PidAllocationError::Invalid);
                }
                let user = namespace.user_namespace();
                if !authority.capable(&user, syscall_abi::capability::Capability::SYS_ADMIN)
                    && !authority.capable(
                        &user,
                        syscall_abi::capability::Capability::CHECKPOINT_RESTORE,
                    )
                {
                    return Err(PidAllocationError::Permission);
                }
            }
            let reservation = namespace.reserve(key, requested.map(|number| number as u32))?;
            numbers.push(Number {
                namespace: namespace.clone(),
                number: reservation.number(),
            });
            reservations.push(reservation);
            current = namespace.parent();
        }
        let identity = Arc::try_new(PidIdentity {
            key,
            numbers,
            registrations: Mutex::new(RegistrationState {
                links: 0,
                phase: Phase::Prepared,
            }),
        })
        .map_err(|_| PidAllocationError::OutOfMemory)?;
        let registration = Some(identity.add_link(link));
        Ok(Self {
            identity,
            registration,
            reservations,
        })
    }
    pub(crate) fn initial() -> Result<Self, PidAllocationError> {
        let link = PidLinkSlot::try_new()?;
        let mut numbers = Vec::new();
        numbers
            .try_reserve_exact(1)
            .map_err(|_| PidAllocationError::OutOfMemory)?;
        numbers.push(Number {
            namespace: crate::namespace::initial_pid(),
            number: 1,
        });
        let identity = Arc::try_new(PidIdentity {
            key: PidKey::INIT,
            numbers,
            registrations: Mutex::new(RegistrationState {
                links: 0,
                phase: Phase::Published,
            }),
        })
        .map_err(|_| PidAllocationError::OutOfMemory)?;
        let registration = Some(identity.add_link(link));
        identity.numbers[0].namespace.publish_initial(&identity);
        Ok(Self {
            identity,
            registration,
            reservations: Vec::new(),
        })
    }
    pub(crate) fn identity(&self) -> Arc<PidIdentity> {
        self.identity.clone()
    }
    /// 必须持唯一 graph publication guard；namespace init disable 同样在此 guard 下执行。
    /// 完整 ladder 复查先于任何 publication；成功后的 links 转移不分配、不释放 storage。
    pub(crate) fn publish(&mut self) -> Result<(), PidAllocationError> {
        if self
            .identity
            .numbers
            .iter()
            .any(|number| !number.namespace.allows_creation())
        {
            return Err(PidAllocationError::Dead);
        }
        for reservation in self.reservations.drain(..) {
            let _ = reservation
                .publish(&self.identity)
                .expect("graph guard excludes namespace init exit during commit");
        }
        self.identity.registrations.lock().phase = Phase::Published;
        Ok(())
    }
    pub(crate) fn take_registration(&mut self) -> PidRegistration {
        self.registration
            .take()
            .expect("prepared PID registration consumed twice")
    }
}

struct PidLink {
    identity: Arc<PidIdentity>,
    next: Option<Box<PidLink>>,
}
impl Drop for PidLink {
    fn drop(&mut self) {
        self.identity.release_link();
    }
}
/// graph PID/TGID/PGID/SID 的实际 ownership link；每条 link storage 在 IRQ guard 外准备。
pub(crate) struct PidRegistration(Box<PidLink>);
impl PidRegistration {
    pub(crate) fn identity(&self) -> &Arc<PidIdentity> {
        &self.0.identity
    }
    pub(crate) fn clone_in(&self, slot: PidLinkSlot) -> Self {
        self.0.identity.add_link(slot)
    }
}
/// 只拥有尚未初始化的 domain link storage；初始化在 graph guard 内不再分配。
pub(crate) struct PidLinkSlot(Box<MaybeUninit<PidLink>>);
impl PidLinkSlot {
    pub(crate) fn try_new() -> Result<Self, PidAllocationError> {
        Box::<PidLink>::try_new_uninit()
            .map(Self)
            .map_err(|_| PidAllocationError::OutOfMemory)
    }
    fn fill(mut self, identity: Arc<PidIdentity>) -> PidRegistration {
        self.0.write(PidLink {
            identity,
            next: None,
        });
        // SAFETY: storage 未发表且被此 token 独占，write 已完整初始化实际 PidLink。
        PidRegistration(unsafe { self.0.assume_init() })
    }
}
/// IRQ guard 内只移交已有 link；链式 storage 的最终析构迭代发生在 guard 外。
#[derive(Default)]
pub(crate) struct RetiredPidRegistrations(Option<Box<PidLink>>);
impl RetiredPidRegistrations {
    pub(crate) fn retire(&mut self, registration: PidRegistration) {
        let mut link = registration.0;
        assert!(link.next.is_none(), "linked registration retired twice");
        link.next = self.0.take();
        self.0 = Some(link);
    }
}
impl Drop for RetiredPidRegistrations {
    fn drop(&mut self) {
        while let Some(mut link) = self.0.take() {
            self.0 = link.next.take();
            drop(link);
        }
    }
}
