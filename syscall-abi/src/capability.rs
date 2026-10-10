//! 固定 Linux v7.1 capability UAPI；只定义编号与 codec，不拥有 credential policy。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability(u8);
impl Capability {
    pub const CHOWN: Self = Self(0);
    pub const DAC_OVERRIDE: Self = Self(1);
    pub const DAC_READ_SEARCH: Self = Self(2);
    pub const FOWNER: Self = Self(3);
    pub const FSETID: Self = Self(4);
    pub const KILL: Self = Self(5);
    pub const SETGID: Self = Self(6);
    pub const SETUID: Self = Self(7);
    pub const SETPCAP: Self = Self(8);
    pub const LINUX_IMMUTABLE: Self = Self(9);
    pub const NET_BIND_SERVICE: Self = Self(10);
    pub const NET_BROADCAST: Self = Self(11);
    pub const NET_ADMIN: Self = Self(12);
    pub const NET_RAW: Self = Self(13);
    pub const IPC_LOCK: Self = Self(14);
    pub const IPC_OWNER: Self = Self(15);
    pub const SYS_MODULE: Self = Self(16);
    pub const SYS_RAWIO: Self = Self(17);
    pub const SYS_CHROOT: Self = Self(18);
    pub const SYS_PTRACE: Self = Self(19);
    pub const SYS_PACCT: Self = Self(20);
    pub const SYS_ADMIN: Self = Self(21);
    pub const SYS_BOOT: Self = Self(22);
    pub const SYS_NICE: Self = Self(23);
    pub const SYS_RESOURCE: Self = Self(24);
    pub const SYS_TIME: Self = Self(25);
    pub const SYS_TTY_CONFIG: Self = Self(26);
    pub const MKNOD: Self = Self(27);
    pub const LEASE: Self = Self(28);
    pub const AUDIT_WRITE: Self = Self(29);
    pub const AUDIT_CONTROL: Self = Self(30);
    pub const SETFCAP: Self = Self(31);
    pub const MAC_OVERRIDE: Self = Self(32);
    pub const MAC_ADMIN: Self = Self(33);
    pub const SYSLOG: Self = Self(34);
    pub const WAKE_ALARM: Self = Self(35);
    pub const BLOCK_SUSPEND: Self = Self(36);
    pub const AUDIT_READ: Self = Self(37);
    pub const PERFMON: Self = Self(38);
    pub const BPF: Self = Self(39);
    pub const CHECKPOINT_RESTORE: Self = Self(40);
    pub fn from_raw(raw: usize) -> Option<Self> {
        (raw <= 40).then_some(Self(raw as u8))
    }
    pub const fn mask(self) -> u64 {
        1u64 << self.0
    }
}
pub const VALID_MASK: u64 = (1u64 << 41) - 1;
pub const VERSION_1: u32 = 0x19980330;
pub const VERSION_2: u32 = 0x20071026;
pub const VERSION_3: u32 = 0x20080522;
/// Linux header version 选择的 data struct 数量；v2/v3 共用同一 codec。
pub fn data_size(version: u32) -> Option<usize> {
    match version {
        VERSION_1 => Some(12),
        VERSION_2 | VERSION_3 => Some(24),
        _ => None,
    }
}
/// capget/capset 的同一 immutable scalar snapshot。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilitySets {
    pub effective: u64,
    pub permitted: u64,
    pub inheritable: u64,
}
impl CapabilitySets {
    /// 单一 12/24-byte Linux layout，非法 size 不解码；未知 capability bits 按 kernel mask 忽略。
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 12 && bytes.len() != 24 {
            return None;
        }
        let mut values = [0u64; 3];
        for (word, block) in bytes.as_chunks::<12>().0.iter().enumerate() {
            for (column, value) in values.iter_mut().enumerate() {
                *value |= u64::from(u32::from_ne_bytes(
                    block[column * 4..column * 4 + 4].try_into().ok()?,
                )) << (word * 32);
            }
        }
        Some(Self {
            effective: values[0] & VALID_MASK,
            permitted: values[1] & VALID_MASK,
            inheritable: values[2] & VALID_MASK,
        })
    }
    /// 完整新版本 buffer；旧版本 caller 只复制首个 data struct。
    pub fn encode(self) -> [u8; 24] {
        let mut result = [0; 24];
        for word in 0..2 {
            for (column, value) in [self.effective, self.permitted, self.inheritable]
                .into_iter()
                .enumerate()
            {
                result[word * 12 + column * 4..word * 12 + column * 4 + 4]
                    .copy_from_slice(&((value >> (word * 32)) as u32).to_ne_bytes());
            }
        }
        result
    }
}
/// proc credential owner 的 readonly five-set 投影，顺序对应 Inh/Prm/Eff/Bnd/Amb。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    pub sets: CapabilitySets,
    pub bounding: u64,
    pub ambient: u64,
}
