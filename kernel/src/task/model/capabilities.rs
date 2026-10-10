//! Credentials 的同锁 scalar capability owner；固定 Linux commoncap 的 set/UID/exec/prctl policy。
use syscall_abi::capability::{Capability, CapabilitySets, CapabilitySnapshot, VALID_MASK};
use syscall_abi::file_capability::{self, FileCapabilityData};

const NOROOT: u32 = 1;
const NO_SETUID_FIXUP: u32 = 1 << 2;
const KEEP_CAPS: u32 = 1 << 4;
const KEEP_CAPS_LOCKED: u32 = KEEP_CAPS << 1;
const NO_AMBIENT_RAISE: u32 = 1 << 6;
const SECURE_BITS: u32 = 0x555;
const SECURE_LOCKS: u32 = SECURE_BITS << 1;
const UNPRIVILEGED: u32 = (1 << 8) | (1 << 10);

/// Calling Thread 的 capability sets 与 securebits；只能作为 Credentials 的同锁字段发布。
#[derive(Debug, Clone, Copy)]
pub(super) struct CapabilityState {
    sets: CapabilitySets,
    bounding: u64,
    ambient: u64,
    securebits: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityError {
    Permission,
    Invalid,
}

/// credential owner 提供的 exec ID facts；group membership 已在同一快照内判断。
pub(super) struct CapabilityExecIds {
    pub(super) uids: [u32; 2],
    pub(super) gids: [u32; 2],
    pub(super) root: Option<u32>,
    pub(super) id_changed: bool,
    pub(super) nnp: bool,
    pub(super) file: Option<FileCapabilityData>,
}

pub(super) struct ExecCapabilityOutcome {
    pub(super) downgrade_ids: bool,
    pub(super) secure: bool,
}

impl CapabilityState {
    pub(super) const fn root() -> Self {
        Self {
            sets: CapabilitySets {
                effective: VALID_MASK,
                permitted: VALID_MASK,
                inheritable: 0,
            },
            bounding: VALID_MASK,
            ambient: 0,
            securebits: 0,
        }
    }
    pub(super) fn capable(&self, capability: Capability) -> bool {
        self.sets.effective & capability.mask() != 0
    }
    pub(super) fn snapshot(&self) -> CapabilitySnapshot {
        CapabilitySnapshot {
            sets: self.sets,
            bounding: self.bounding,
            ambient: self.ambient,
        }
    }
    /// faccessat real-ID 检查按固定 Linux 选择 permitted 或清空 effective，除非 NO_SETUID_FIXUP。
    pub(super) fn access_effective(
        &self,
        effective: bool,
        real_uid: u32,
        root: Option<u32>,
    ) -> u64 {
        if effective || self.securebits & NO_SETUID_FIXUP != 0 {
            self.sets.effective
        } else if root.is_some_and(|root| real_uid == root) {
            self.sets.permitted
        } else {
            0
        }
    }
    /// 所有约束在修改前复查；pE⊆pP，pP不可增加，pI同时受旧pI/pP/bset约束。
    pub(super) fn set(&mut self, new: CapabilitySets) -> Result<(), CapabilityError> {
        let old = self.sets;
        if (!self.capable(Capability::SETPCAP)
            && new.inheritable & !(old.inheritable | old.permitted) != 0)
            || new.inheritable & !(old.inheritable | self.bounding) != 0
            || new.permitted & !old.permitted != 0
            || new.effective & !new.permitted != 0
        {
            return Err(CapabilityError::Permission);
        }
        self.sets = new;
        self.ambient &= new.permitted & new.inheritable;
        Ok(())
    }
    /// set*uid 的同一 credential commit 后调用；root ID 是当前 user namespace 的 kuid 映射。
    pub(super) fn fix_setuid(&mut self, old: [u32; 3], new: [u32; 3], root: Option<u32>) {
        if self.securebits & NO_SETUID_FIXUP != 0 {
            return;
        }
        let Some(root) = root else { return };
        if old.contains(&root) && !new.contains(&root) {
            if self.securebits & KEEP_CAPS == 0 {
                self.sets.permitted = 0;
                self.sets.effective = 0;
            }
            self.ambient = 0;
        }
        if old[1] == root && new[1] != root {
            self.sets.effective = 0;
        }
        if old[1] != root && new[1] == root {
            self.sets.effective = self.sets.permitted;
        }
    }
    /// commoncap唯一exec transition：file P/I、bounding、ambient、root与NNP共用scalar owner。
    /// effective文件不能获得全部file-permitted位时在修改前返回Permission，禁止静默降权执行。
    pub(super) fn exec(
        &mut self,
        ids: CapabilityExecIds,
    ) -> Result<ExecCapabilityOutcome, CapabilityError> {
        let CapabilityExecIds {
            uids: [real_uid, effective_uid],
            gids: [real_gid, effective_gid],
            root,
            id_changed,
            nnp,
            file,
        } = ids;
        let old_permitted = self.sets.permitted;
        let root_legacy = self.securebits & NOROOT == 0;
        let real_root = root.is_some_and(|root| real_uid == root);
        let effective_root = root.is_some_and(|root| effective_uid == root);
        let mut effective = file.is_some_and(|file| file.magic & file_capability::EFFECTIVE != 0);
        let mut permitted = 0;
        if let Some(file) = file {
            let file_permitted = file.permitted & VALID_MASK;
            permitted = (self.bounding & file_permitted)
                | (self.sets.inheritable & file.inheritable & VALID_MASK);
            if effective && file_permitted & !permitted != 0 {
                return Err(CapabilityError::Permission);
            }
        }
        // 有file-cap的setuid-root/non-root real UID不叠加legacy root权限；真实root仍按commoncap覆盖。
        if root_legacy && !(file.is_some() && !real_root && effective_root) {
            if real_root || effective_root {
                permitted = self.bounding | self.sets.inheritable;
            }
            if effective_root {
                effective = true;
            }
        }
        let downgrade_ids = nnp && (id_changed || permitted & !old_permitted != 0);
        if downgrade_ids {
            permitted &= old_permitted;
        }
        if id_changed || file.is_some() {
            self.ambient = 0;
        }
        self.sets.permitted = permitted | self.ambient;
        self.sets.effective = if effective {
            self.sets.permitted
        } else {
            self.ambient
        };
        self.securebits &= !KEEP_CAPS;
        let final_uid = if downgrade_ids {
            real_uid
        } else {
            effective_uid
        };
        let final_gid = if downgrade_ids {
            real_gid
        } else {
            effective_gid
        };
        Ok(ExecCapabilityOutcome {
            downgrade_ids,
            secure: id_changed
                || final_uid != real_uid
                || final_gid != real_gid
                || (!real_root && (effective || self.sets.permitted & !self.ambient != 0)),
        })
    }
    /// 固定 commoncap LSM_SETID_FS：只调整历史 filesystem effective sets。
    pub(super) fn fix_fsuid(&mut self, old: u32, new: u32, root: Option<u32>) {
        if self.securebits & NO_SETUID_FIXUP != 0 {
            return;
        }
        let Some(root) = root else { return };
        const FS_SET: u64 = Capability::CHOWN.mask()
            | Capability::MKNOD.mask()
            | Capability::DAC_OVERRIDE.mask()
            | Capability::DAC_READ_SEARCH.mask()
            | Capability::FOWNER.mask()
            | Capability::FSETID.mask()
            | Capability::MAC_OVERRIDE.mask()
            | Capability::LINUX_IMMUTABLE.mask();
        if old == root && new != root {
            self.sets.effective &= !FS_SET;
        }
        if old != root && new == root {
            self.sets.effective |= self.sets.permitted & FS_SET;
        }
    }

    /// capability prctl 单一入口；无 user-copy/分配，保持 fixed commoncap 参数与权限错误顺序。
    pub(super) fn prctl(
        &mut self,
        option: u32,
        argument: usize,
        remaining: [usize; 3],
    ) -> Result<usize, CapabilityError> {
        match option {
            7 => Ok(usize::from(self.securebits & KEEP_CAPS != 0)),
            8 => {
                if argument > 1 {
                    return Err(CapabilityError::Invalid);
                }
                if self.securebits & KEEP_CAPS_LOCKED != 0 {
                    return Err(CapabilityError::Permission);
                }
                self.securebits = self.securebits & !KEEP_CAPS | (argument as u32 * KEEP_CAPS);
                Ok(0)
            }
            23 => Ok(usize::from(
                self.bounding & Self::cap(argument)?.mask() != 0,
            )),
            24 => {
                if !self.capable(Capability::SETPCAP) {
                    return Err(CapabilityError::Permission);
                }
                self.bounding &= !Self::cap(argument)?.mask();
                Ok(0)
            }
            27 => Ok(self.securebits as usize),
            28 => {
                let changed = self.securebits as usize ^ argument;
                if argument & !((SECURE_BITS | SECURE_LOCKS) as usize) != 0
                    || changed & ((self.securebits & SECURE_LOCKS) >> 1) as usize != 0
                    || self.securebits as usize & SECURE_LOCKS as usize & !argument != 0
                    || (!self.capable(Capability::SETPCAP)
                        && (changed == 0
                            || changed & !((UNPRIVILEGED | (UNPRIVILEGED << 1)) as usize) != 0))
                {
                    return Err(CapabilityError::Permission);
                }
                self.securebits = argument as u32;
                Ok(0)
            }
            47 => {
                if argument == 4 {
                    if remaining != [0; 3] {
                        return Err(CapabilityError::Invalid);
                    }
                    self.ambient = 0;
                    return Ok(0);
                }
                if remaining[1] != 0 || remaining[2] != 0 {
                    return Err(CapabilityError::Invalid);
                }
                let mask = Self::cap(remaining[0])?.mask();
                match argument {
                    1 => Ok(usize::from(self.ambient & mask != 0)),
                    2 => {
                        if self.sets.permitted & self.sets.inheritable & mask == 0
                            || self.securebits & NO_AMBIENT_RAISE != 0
                        {
                            return Err(CapabilityError::Permission);
                        }
                        self.ambient |= mask;
                        Ok(0)
                    }
                    3 => {
                        self.ambient &= !mask;
                        Ok(0)
                    }
                    _ => Err(CapabilityError::Invalid),
                }
            }
            _ => Err(CapabilityError::Invalid),
        }
    }
    fn cap(raw: usize) -> Result<Capability, CapabilityError> {
        Capability::from_raw(raw).ok_or(CapabilityError::Invalid)
    }
}
