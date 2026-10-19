const ENABLE_CONTEXT_BYTES: usize = 0x80;
const BITS_PER_BYTE: usize = 8;

/// PLIC 标准 register geometry 可编码的最大 interrupt vector。
pub(super) const MAX_INTERRUPT_VECTOR: u32 = (ENABLE_CONTEXT_BYTES * BITS_PER_BYTE - 1) as u32;

/// 单次 external hardirq 最多处理的 claim 数。
///
/// 固定上限防止持续中断源把 hardirq 变成无界循环；未 claim 的 pending source 仍由
/// PLIC 保持 pending，并在返回后自然再次触发 external interrupt。
pub(super) const HARDIRQ_CLAIM_BUDGET: usize = 64;

/// 判断 vector 是否可由 PLIC 标准 register geometry 表示。
///
/// `0` 是 claim/complete 接口的“无中断”哨兵，不是可注册的 source。
///
/// # Parameters
///
/// - `vector`: 待验证的 PLIC interrupt source ID。
///
/// # Returns
///
/// 仅当 `vector` 位于 `1..=1023` 时返回 `true`。
pub(super) const fn valid_interrupt_vector(vector: u32) -> bool {
    vector != 0 && vector <= MAX_INTERRUPT_VECTOR
}

/// 计算 vector 在单个 context enable bitmap 内的 word byte offset。
///
/// # Parameters
///
/// - `vector`: 待编码的 PLIC interrupt source ID。
///
/// # Returns
///
/// 合法 vector 的 word offset；`0` 或越过 context stride 时返回 `None`。
pub(super) const fn enable_word_offset(vector: u32) -> Option<usize> {
    if valid_interrupt_vector(vector) {
        Some((vector / u32::BITS) as usize * core::mem::size_of::<u32>())
    } else {
        None
    }
}

/// 在固定 hardirq 预算内 claim、处理并 complete 外部中断。
///
/// # Parameters
///
/// - `claim`: 返回下一个 vector，`0` 表示当前没有更多 pending source。
/// - `handle`: 处理一个已 claim vector。
/// - `complete`: 向控制器完成一个已 claim vector。
///
/// # Returns
///
/// 全部 handler 成功时返回 `Ok(())`，否则在完成本批次后返回第一个错误。
///
/// # Errors
///
/// 返回本批次第一个 handler error；该 vector 仍会且只会 complete 一次。
pub(super) fn dispatch_claim_batch<E>(
    mut claim: impl FnMut() -> u32,
    mut handle: impl FnMut(u32) -> Result<(), E>,
    mut complete: impl FnMut(u32),
) -> Result<(), E> {
    let mut first_error = None;
    for _ in 0..HARDIRQ_CLAIM_BUDGET {
        // 1. 只 claim 本批预算内的 source，不为探测 backlog 预读额外 vector。
        let vector = claim();
        if vector == 0 {
            break;
        }

        // 2. handler error 不得跳过 complete；先完成 hardware cleanup，再保留首错。
        let result = handle(vector);
        complete(vector);
        if first_error.is_none() {
            first_error = result.err();
        }
        // 3. 预算耗尽直接返回；未 claim source 继续由 PLIC 保持 pending。
    }
    first_error.map_or(Ok(()), Err)
}
