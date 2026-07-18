/// deferred/synchronous line discipline 单批最多消费的 raw input 字节数。
pub(crate) const TERMINAL_INPUT_BATCH_BYTES: usize = 256;

/// 非 PTY character backend 保持的 syscall user-copy 分块上限。
pub(crate) const CHARACTER_WRITE_CHUNK_BYTES: usize = 512;

/// @description 选择 character syscall 的单次 user-copy/backend write 上限。
/// @param remaining 当前 syscall 尚未处理的字节数。
/// @param pty_master backend 是否为 PTY master。
/// @return PTY master 不超过 line-discipline 256-byte budget；其他 character 不超过 512 bytes。
pub(crate) fn character_write_chunk(remaining: usize, pty_master: bool) -> usize {
    remaining.min(if pty_master {
        TERMINAL_INPUT_BATCH_BYTES
    } else {
        CHARACTER_WRITE_CHUNK_BYTES
    })
}

/// @description 计算本批下一次 raw read 的固定上限。
/// @param consumed 本批已经消费的字节数。
/// @param buffer_capacity caller 栈上 raw buffer 容量。
/// @return 不超过剩余 256-byte budget 与 buffer 的读取长度；预算耗尽返回零。
pub(crate) fn terminal_input_chunk(consumed: usize, buffer_capacity: usize) -> usize {
    TERMINAL_INPUT_BATCH_BYTES
        .saturating_sub(consumed)
        .min(buffer_capacity)
}

/// @description 一次固定输入批次的锁外后续动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalInputBatch {
    /// 本批输入生成的 Linux signal bitset。
    pub(crate) signals: u64,
    /// raw Console adapter 在批次结束后仍有输入。
    pub(crate) backlog: bool,
}
