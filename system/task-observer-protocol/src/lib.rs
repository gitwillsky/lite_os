#![no_std]
//! LiteOS 系统任务观测消息。只读快照，不传递 capability，不提供终止或重启操作。
//!
//! 这是授予指定系统程序的服务协议，不是一般应用 ABI：
//! 1. 端点是 linuxd 在启动 init 之前创建的 pathname datagram socket [`PATH`]，属主 root、组 [`GROUP`]、
//!    权限 0660；`connect`/`sendto` 按 Linux 规则检查 socket inode 写权限（fsuid 与附加组），
//!    不在组内的进程得到 `EACCES`。
//! 2. 快照覆盖根 Job 下的全部原生进程；Linux 进程已由 /proc 呈现，不在此重复。

/// 服务端点路径。linuxd 自己挂载的 devtmpfs 在 init 之前已存在（同 `/dev/log` 的先例），
/// `/run` 由 init 脚本稍后挂载，内核侧无法提前在那里发布。
pub const PATH: &[u8] = b"/dev/task-observer";
/// 允许查询的组：Linux 发行版惯用的系统监视组 `adm`（Alpine/Debian 均为 GID 4）。
pub const GROUP: u32 = 4;
/// 一个请求恰为这八个字节；版本变化必须改变 magic，不猜测未知布局。
pub const QUERY: &[u8; 8] = b"LTASK002";
/// 每条记录的编码字节数；整数为 little endian，名称为 NUL 填充 UTF-8。
pub const RECORD_BYTES: usize = 104;
/// 回复头：magic 与 u32 记录数。
pub const HEADER_BYTES: usize = 12;
/// 查询失败的指标；与真实零值严格区分。
pub const UNAVAILABLE: u64 = u64::MAX;

/// 原生进程记录。KOID 不属于 Linux PID 空间，禁止用于 kill(2)。
#[derive(Clone, Debug, Default)]
pub struct Record {
    /// 内核对象的稳定身份。
    pub koid: u64,
    /// 包括已退出线程的累计 CPU 纳秒。
    pub cpu_ns: u64,
    /// 实际 RAM 驻留字节，不是 PSS，不能跨进程直接相加。
    pub resident_bytes: u64,
    /// 仅有一个逻辑映射的驻留字节。
    pub private_bytes: u64,
    /// 多个逻辑映射共享的驻留字节，不可跨行相加。
    pub shared_bytes: u64,
    /// 私有驻留加逐页分摊的共享驻留（PSS）。
    pub pss_bytes: u64,
    /// 映射区间字节和。
    pub mapped_bytes: u64,
    /// 存活线程数。
    pub threads: u64,
    /// 0 为运行中，1 为已退出，2 为查询失败。
    pub state: u64,
    /// 内核对象名。
    pub name: [u8; 32],
}

impl Record {
    /// 编码恰好一条记录；输出长度不符返回 false，不产生部分消息。
    pub fn encode(&self, output: &mut [u8]) -> bool {
        if output.len() != RECORD_BYTES {
            return false;
        }
        let fields = [
            self.koid,
            self.cpu_ns,
            self.resident_bytes,
            self.private_bytes,
            self.shared_bytes,
            self.pss_bytes,
            self.mapped_bytes,
            self.threads,
            self.state,
        ];
        for (bytes, value) in output[..72].as_chunks_mut::<8>().0.iter_mut().zip(fields) {
            bytes.copy_from_slice(&value.to_le_bytes());
        }
        output[72..].copy_from_slice(&self.name);
        true
    }

    /// 校验并解码一条记录；拒绝长度错误、未知状态、无效 KOID 或名称。
    pub fn decode(input: &[u8]) -> Option<Self> {
        if input.len() != RECORD_BYTES {
            return None;
        }
        let mut fields = [0; 9];
        for (value, bytes) in fields.iter_mut().zip(input[..72].as_chunks::<8>().0) {
            *value = u64::from_le_bytes(*bytes);
        }
        if fields[0] == 0 || fields[8] > 2 {
            return None;
        }
        let name: [u8; 32] = input[72..].try_into().ok()?;
        core::str::from_utf8(&name[..name.iter().position(|byte| *byte == 0).unwrap_or(32)])
            .ok()?;
        Some(Self {
            koid: fields[0],
            cpu_ns: fields[1],
            resident_bytes: fields[2],
            private_bytes: fields[3],
            shared_bytes: fields[4],
            pss_bytes: fields[5],
            mapped_bytes: fields[6],
            threads: fields[7],
            state: fields[8],
            name,
        })
    }
}
