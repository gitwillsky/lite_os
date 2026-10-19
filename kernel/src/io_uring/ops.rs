//! SQE 的通用 init 校验与不依赖 fd 表的 opcode（NOP）。
//!
//! 带 fd 的 opcode（读写、fsync、close）需要提交者的 fd 表、地址空间和读写核心，它们属于 `syscall`；
//! 本模块经 [`Submitter`] 把它们交给提交者，自己只保证校验顺序与 Linux 一致。
//!
//! 错误分两类，与 Linux 一致：init/prep 阶段失败（flags 非法、opcode 不支持、字段非法）后不再提交
//! 后续 SQE（[`Flow::Stop`]）；issue 阶段的失败（如注入的负结果、无效 fd）只体现在该 CQE 的 `res`。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::{
    abi::{
        IOSQE_BUFFER_SELECT, IOSQE_CQE_SKIP_SUCCESS, IOSQE_IO_DRAIN, IOSQE_IO_HARDLINK,
        IOSQE_IO_LINK, NOP_INJECT_RESULT, OP_LAST, OP_NOP, SQE_VALID_FLAGS, Sqe, op_supported,
    },
    ring::{Flow, Ring},
};

const NOP_FILE: u32 = 1 << 1;
const NOP_FIXED_FILE: u32 = 1 << 2;
const NOP_FIXED_BUFFER: u32 = 1 << 3;
const NOP_TW: u32 = 1 << 4;
const NOP_CQE32: u32 = 1 << 5;
const NOP_FLAGS: u32 =
    NOP_INJECT_RESULT | NOP_FILE | NOP_FIXED_FILE | NOP_FIXED_BUFFER | NOP_TW | NOP_CQE32;

/// 提交者提供的能力：fd 表、地址空间与读写核心都在 `syscall`，ring 域不依赖它们。
pub(crate) trait Submitter {
    /// `fd` 在提交者 fd 表中是否存在（`IORING_NOP_FILE`）。
    fn fd_exists(&self, fd: i32) -> bool;

    /// 执行需要 fd 表、地址空间或挂起能力的 opcode。实现负责 prep 校验、同步尝试，以及发布 CQE
    /// 或把请求挂起；init/prep 阶段失败返回 [`Flow::Stop`]。
    fn issue_op(&self, ring: &Arc<Ring>, sqe: Sqe) -> Flow;
}

/// 发布 init/prep 失败的 CQE 并停止本次提交。
pub(crate) fn fail_init(ring: &Ring, sqe: &Sqe, error: isize) -> Flow {
    ring.complete(sqe.user_data, false, -(error as i32));
    Flow::Stop
}

/// 校验并执行一个 SQE。
pub(crate) fn issue(ring: &Arc<Ring>, sqe: Sqe, submitter: &dyn Submitter) -> Flow {
    if sqe.opcode >= OP_LAST {
        return fail_init(ring, &sqe, errno::EINVAL);
    }
    if sqe.flags & !SQE_VALID_FLAGS != 0 {
        return fail_init(ring, &sqe, errno::EINVAL);
    }
    // 没有任何已实现的 opcode 支持 buffer-select；Linux 对“已知但该 op 不支持”返回 EOPNOTSUPP。
    if sqe.flags & IOSQE_BUFFER_SELECT != 0 {
        return fail_init(ring, &sqe, errno::EOPNOTSUPP);
    }
    // 链式提交尚未实现：静默忽略 LINK 会让“前一个失败则取消后续”的语义丢失，所以显式拒绝。
    if sqe.flags & (IOSQE_IO_LINK | IOSQE_IO_HARDLINK) != 0 {
        return fail_init(ring, &sqe, errno::EOPNOTSUPP);
    }
    // 已有挂起请求（阻塞型读写）而 DRAIN 的排序语义尚未实现：静默忽略会让“等所有在途请求完成后
    // 再执行”的承诺落空，所以显式拒绝，Linux 在 DRAIN 不可用时同样返回 EOPNOTSUPP。
    if sqe.flags & IOSQE_IO_DRAIN != 0 {
        return fail_init(ring, &sqe, errno::EOPNOTSUPP);
    }
    if !op_supported(sqe.opcode) {
        return fail_init(ring, &sqe, errno::EOPNOTSUPP);
    }
    // personality 需要注册表（io_uring_register PERSONALITY），尚未实现；使用提交者的当前凭据。
    if sqe.personality != 0 {
        return fail_init(ring, &sqe, errno::EINVAL);
    }
    match sqe.opcode {
        OP_NOP => nop(ring, sqe, submitter),
        _ => submitter.issue_op(ring, sqe),
    }
}

fn nop(ring: &Ring, sqe: Sqe, submitter: &dyn Submitter) -> Flow {
    if sqe.ioprio != 0 || sqe.op_flags & !NOP_FLAGS != 0 {
        return fail_init(ring, &sqe, errno::EINVAL);
    }
    // ring 没有 CQE32/CQE_MIXED，所以 NOP_CQE32 与 Linux 一样是 EINVAL。
    if sqe.op_flags & NOP_CQE32 != 0 {
        return fail_init(ring, &sqe, errno::EINVAL);
    }
    let mut result: i32 = if sqe.op_flags & NOP_INJECT_RESULT != 0 {
        sqe.len as i32
    } else {
        0
    };
    if sqe.op_flags & NOP_FILE != 0 {
        let fixed = sqe.op_flags & NOP_FIXED_FILE != 0;
        // 没有注册文件表，所以 fixed 引用恒为 EBADF。
        if fixed || !submitter.fd_exists(sqe.fd) {
            result = -(errno::EBADF as i32);
        }
    }
    // 没有注册缓冲区，所以 fixed buffer 引用恒为 EFAULT（Linux：`io_find_buf_node` 失败）。
    if sqe.op_flags & NOP_FIXED_BUFFER != 0 && result >= 0 {
        result = -(errno::EFAULT as i32);
    }
    // NOP_TW 只改变完成发生在哪个 task context；这里同步完成，结果相同。
    let _ = NOP_TW;
    ring.complete(
        sqe.user_data,
        sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0,
        result,
    );
    Flow::Continue
}
