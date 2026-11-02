#![no_std]
//! LiteOS 系统任务观测消息。只读快照，不传递 capability，不提供终止或重启操作。

/// 抽象 AF_UNIX 地址（不含前导 NUL）；仅系统的初始网络命名空间发布。
pub const ADDRESS: &[u8] = b"org.liteos.task-observer";
/// 一个请求恰为这八个字节；版本变化必须改变 magic，不猜测未知布局。
pub const QUERY: &[u8; 8] = b"LTASK001";
/// 每条记录的编码字节数；整数为 little endian，名称为 NUL 填充 UTF-8。
pub const RECORD_BYTES: usize = 80;
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
    /// 映射驻留字节，不是 PSS，不能跨进程直接相加。
    pub resident_bytes: u64,
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
            self.mapped_bytes,
            self.threads,
            self.state,
        ];
        for (bytes, value) in output[..48].as_chunks_mut::<8>().0.iter_mut().zip(fields) {
            bytes.copy_from_slice(&value.to_le_bytes());
        }
        output[48..].copy_from_slice(&self.name);
        true
    }

    /// 校验并解码一条记录；拒绝长度错误、未知状态、无效 KOID 或名称。
    pub fn decode(input: &[u8]) -> Option<Self> {
        if input.len() != RECORD_BYTES {
            return None;
        }
        let mut fields = [0; 6];
        for (value, bytes) in fields.iter_mut().zip(input[..48].as_chunks::<8>().0) {
            *value = u64::from_le_bytes(*bytes);
        }
        if fields[0] == 0 || fields[5] > 2 {
            return None;
        }
        let name: [u8; 32] = input[48..].try_into().ok()?;
        core::str::from_utf8(&name[..name.iter().position(|byte| *byte == 0).unwrap_or(32)])
            .ok()?;
        Some(Self {
            koid: fields[0],
            cpu_ns: fields[1],
            resident_bytes: fields[2],
            mapped_bytes: fields[3],
            threads: fields[4],
            state: fields[5],
            name,
        })
    }
}
