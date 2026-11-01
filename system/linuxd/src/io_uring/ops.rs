//! SQE 的通用 init 校验与 prep：把一个 SQE 解码成链的成员（[`Member`]）。
//!
//! 带 fd 的 opcode（读写、fsync、close…）需要提交者的 fd 表、地址空间和读写核心，它们的 prep 与执行
//! 属于 `syscall`；本模块经 [`Submitter`] 把它们交给提交者，自己只保证校验顺序与 Linux 一致，并处理不依赖
//! fd 表的 NOP。
//!
//! init/prep 失败（flags 非法、opcode 不支持、字段非法）的成员是 [`Step::Failed`]：它所在的链不执行，
//! 且本次提交在链结束处停止（见 `chain`）。issue 阶段的失败（如注入的负结果、无效 fd）只体现在该 CQE 的
//! `res`，并按链规则取消后继。

use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use syscall_abi::errno;

use super::{
    abi::{
        IOSQE_BUFFER_SELECT, IOSQE_CQE_SKIP_SUCCESS, IOSQE_IO_DRAIN, IOSQE_IO_HARDLINK,
        IOSQE_IO_LINK, NOP_INJECT_RESULT, OP_LAST, OP_LINK_TIMEOUT, OP_NOP, SQE_VALID_FLAGS, Sqe,
        op_supported,
    },
    chain::{Chain, Link, Member, Nop, Step},
    ring::Ring,
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
    /// prep 后拥有全部用户数据的 op（读写、fsync、close、poll、timeout、cancel…）。
    type Op;

    /// 校验并解码需要 fd 表、地址空间或挂起能力的 opcode；失败返回正 errno。
    fn prepare_op(&self, ring: &Ring, sqe: &Sqe) -> Result<Self::Op, isize>;

    /// 开始执行一条已组装、未失败的链：成员按序 issue，直到某个成员需要等待，其余随之挂起。
    fn launch(&self, ring: &Arc<Ring>, chain: Chain<Self::Op>);
}

/// 校验并解码一个 SQE。
pub(crate) fn prepare<S: Submitter>(ring: &Ring, sqe: &Sqe, submitter: &S) -> Member<S::Op> {
    let step = match decode(ring, sqe, submitter) {
        Ok(step) => step,
        Err(error) => Step::Failed(-(error as i32)),
    };
    let link = if sqe.flags & IOSQE_IO_HARDLINK != 0 {
        Link::Hard
    } else if sqe.flags & IOSQE_IO_LINK != 0 {
        Link::Soft
    } else {
        Link::None
    };
    Member::new(
        sqe.user_data,
        sqe.opcode,
        sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0,
        link,
        sqe.flags & IOSQE_IO_DRAIN != 0,
        step,
    )
}

fn decode<S: Submitter>(ring: &Ring, sqe: &Sqe, submitter: &S) -> Result<Step<S::Op>, isize> {
    if sqe.opcode >= OP_LAST {
        return Err(errno::EINVAL);
    }
    if sqe.flags & !SQE_VALID_FLAGS != 0 {
        return Err(errno::EINVAL);
    }
    // 没有任何已实现的 opcode 支持 buffer-select；Linux 对“已知但该 op 不支持”返回 EOPNOTSUPP。
    if sqe.flags & IOSQE_BUFFER_SELECT != 0 {
        return Err(errno::EOPNOTSUPP);
    }
    // 被跳过的 CQE 使“全部完成”无从由 CQ 观察，所以一旦用过 SKIP_SUCCESS，ring 上的 DRAIN 恒为
    // EOPNOTSUPP（Linux `IO_RING_F_DRAIN_DISABLED`）。
    if sqe.flags & IOSQE_CQE_SKIP_SUCCESS != 0 {
        ring.drain_disabled().store(true, Ordering::Release);
    }
    if sqe.flags & IOSQE_IO_DRAIN != 0 && ring.drain_disabled().load(Ordering::Acquire) {
        return Err(errno::EOPNOTSUPP);
    }
    if !op_supported(sqe.opcode) {
        return Err(errno::EOPNOTSUPP);
    }
    // personality 需要注册表（io_uring_register PERSONALITY），尚未实现；使用提交者的当前凭据。
    if sqe.personality != 0 {
        return Err(errno::EINVAL);
    }
    match sqe.opcode {
        OP_NOP => nop(sqe).map(Step::Nop),
        OP_LINK_TIMEOUT => submitter.prepare_op(ring, sqe).map(Step::LinkTimeout),
        _ => submitter.prepare_op(ring, sqe).map(Step::Op),
    }
}

fn nop(sqe: &Sqe) -> Result<Nop, isize> {
    if sqe.ioprio != 0 || sqe.op_flags & !NOP_FLAGS != 0 {
        return Err(errno::EINVAL);
    }
    // ring 没有 CQE32/CQE_MIXED，所以 NOP_CQE32 与 Linux 一样是 EINVAL。
    if sqe.op_flags & NOP_CQE32 != 0 {
        return Err(errno::EINVAL);
    }
    // NOP_TW 只改变完成发生在哪个 task context；这里同步完成，结果相同。
    let _ = NOP_TW;
    Ok(Nop {
        result: if sqe.op_flags & NOP_INJECT_RESULT != 0 {
            sqe.len as i32
        } else {
            0
        },
        file: (sqe.op_flags & NOP_FILE != 0)
            .then_some((sqe.fd, sqe.op_flags & NOP_FIXED_FILE != 0)),
        fixed_buffer: sqe.op_flags & NOP_FIXED_BUFFER != 0,
    })
}
