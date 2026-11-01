use core::sync::atomic::{AtomicBool, Ordering};

/// OWNER: 唯一 mm dumpability；CLONE_VM 共用，fork复制值，exec私有mm先disabled。
///
/// 当前 suid_dumpable 固定为0，PR_SET_DUMPABLE仅允许0/1。Release提交先于credentials
/// publication；ptrace/proc在credential snapshot之后Acquire读取，避免身份降级窗口。
/// 不使用MemorySet阻塞锁，graph权限检查不引入mm锁或分配。
pub(super) struct Dumpability(AtomicBool);
impl Dumpability {
    pub(super) fn new(user: bool) -> Self {
        Self(AtomicBool::new(user))
    }
    pub(super) fn user_dumpable(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub(super) fn set_user_dumpable(&self, user: bool) {
        self.0.store(user, Ordering::Release);
    }
}
