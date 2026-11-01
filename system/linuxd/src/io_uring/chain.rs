//! SQE 链的组装与链失败时的完成规则（Linux `io_submit_sqe`、`io_submit_fail_init`、`io_fail_links`）。
//!
//! 链由带 `IOSQE_IO_LINK`/`IOSQE_IO_HARDLINK` 的 SQE 起头或接续，第一个不带该标志的 SQE 结束它。
//! 本模块只决定“哪些 SQE 组成一条链、prep 失败时怎样完成它们”；成员的执行（含失败后取消后继）属于
//! 提交者（`syscall::io_uring`），因为它们需要 fd 表与地址空间。

use alloc::vec::{self, Vec};
use syscall_abi::errno;

use super::ring::{Flow, Ring};

/// 成员与后继的链接方式。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    /// 链的最后一个成员，或不在链中。
    None,
    /// `IOSQE_IO_LINK`：本成员失败则取消其后全部成员。
    Soft,
    /// `IOSQE_IO_HARDLINK`：本成员失败也继续执行后继。
    Hard,
}

/// `IORING_OP_NOP` 经 prep 校验后的参数；它的 issue 结果取决于提交者的 fd 表，所以在执行时才求值。
pub(crate) struct Nop {
    pub(super) result: i32,
    /// `IORING_NOP_FILE`：`(fd, 是否 fixed 引用)`。
    pub(super) file: Option<(i32, bool)>,
    pub(super) fixed_buffer: bool,
}

impl Nop {
    /// 求出 CQE 的 `res`；`fd_exists` 查询提交者 fd 表。
    pub(crate) fn result(&self, fd_exists: impl FnOnce(i32) -> bool) -> i32 {
        let mut result = self.result;
        if let Some((fd, fixed)) = self.file {
            // 没有注册文件表，所以 fixed 引用恒为 EBADF。
            if fixed || !fd_exists(fd) {
                result = -(errno::EBADF as i32);
            }
        }
        // 没有注册缓冲区，所以 fixed buffer 引用恒为 EFAULT（Linux：`io_find_buf_node` 失败）。
        if self.fixed_buffer && result >= 0 {
            result = -(errno::EFAULT as i32);
        }
        result
    }
}

/// 一个成员要做什么。
pub(crate) enum Step<Op> {
    /// init/prep 失败：以该 `res`（负 errno）完成，所在链的其余成员被取消。
    Failed(i32),
    Nop(Nop),
    Op(Op),
    /// `IORING_OP_LINK_TIMEOUT`：约束它前面那个链成员的执行时间。单独成为一种 step，使链组装能校验它的位置
    /// （必须紧跟在链成员之后）并让执行者在前一个成员开始等待时把它摘下来装成定时器。
    LinkTimeout(Op),
}

/// 链中的一个已 prep 的 SQE。
pub(crate) struct Member<Op> {
    pub(crate) user_data: u64,
    /// SQE 的 opcode：按 opcode 取消（`IORING_ASYNC_CANCEL_OP`）匹配它；缺失时只能按 user_data 取消。
    pub(crate) opcode: u8,
    pub(crate) skip_success: bool,
    pub(crate) link: Link,
    pub(super) drain: bool,
    pub(crate) step: Step<Op>,
}

impl<Op> Member<Op> {
    pub(super) fn new(
        user_data: u64,
        opcode: u8,
        skip_success: bool,
        link: Link,
        drain: bool,
        step: Step<Op>,
    ) -> Self {
        Self {
            user_data,
            opcode,
            skip_success,
            link,
            drain,
            step,
        }
    }
}

/// 一条待执行的链；单个 SQE 是只有 `head` 的链，不分配。
pub(crate) struct Chain<Op> {
    head: Member<Op>,
    rest: Vec<Member<Op>>,
    /// 链中有 DRAIN 成员，或前一条链的中间成员要求其后的链 DRAIN。
    drain: bool,
    /// 某个成员 prep 失败：整条链不执行，只按失败规则完成。
    failed: bool,
}

impl<Op> Chain<Op> {
    pub(crate) fn drain(&self) -> bool {
        self.drain
    }

    pub(crate) fn into_members(self) -> Members<Op> {
        Members {
            head: Some(self.head),
            rest: self.rest.into_iter(),
        }
    }
}

/// 按提交顺序消费链的成员。
pub(crate) struct Members<Op> {
    head: Option<Member<Op>>,
    rest: vec::IntoIter<Member<Op>>,
}

impl<Op> Members<Op> {
    /// 下一个成员是 `LINK_TIMEOUT` 时取走它，否则不消费任何成员。
    pub(crate) fn next_if_link_timeout(&mut self) -> Option<Member<Op>> {
        let is_timeout = |member: &Member<Op>| matches!(member.step, Step::LinkTimeout(_));
        let next = match &self.head {
            Some(head) => head,
            None => self.rest.as_slice().first()?,
        };
        if is_timeout(next) { self.next() } else { None }
    }
}

impl<Op> Iterator for Members<Op> {
    type Item = Member<Op>;

    fn next(&mut self) -> Option<Member<Op>> {
        self.head.take().or_else(|| self.rest.next())
    }
}

/// 以 `-ECANCELED` 完成 `rest` 里剩下的成员；`skip` 为真时不发布它们的 CQE。
pub(crate) fn cancel_rest<Op>(ring: &Ring, rest: impl Iterator<Item = Member<Op>>, skip: bool) {
    for member in rest {
        ring.complete(member.user_data, skip, -(errno::ECANCELED as i32));
    }
}

/// 完成一条因 prep 失败而不执行的链：失败成员得到自己的错误，其余成员 `-ECANCELED`。
///
/// 与 Linux 一致，链头自己的 `IOSQE_CQE_SKIP_SUCCESS` 使其后全部成员的 CQE 被跳过，链头的 CQE 总是发布。
fn fail_chain<Op>(ring: &Ring, chain: Chain<Op>) {
    let skip_rest = chain.head.skip_success;
    for (index, member) in chain.into_members().enumerate() {
        let res = match member.step {
            Step::Failed(res) => res,
            _ => -(errno::ECANCELED as i32),
        };
        ring.complete(member.user_data, index != 0 && skip_rest, res);
    }
}

/// 一次提交批里正在组装的链。
pub(crate) struct Assembler<Op> {
    open: Option<Chain<Op>>,
}

impl<Op> Assembler<Op> {
    pub(crate) fn new() -> Self {
        Self { open: None }
    }

    /// 接纳下一个成员。返回可以执行的链（若本成员结束了它）与是否继续消费 SQE。
    ///
    /// prep 失败的成员在链中不中断组装（让链的剩余成员一并被取消），只有当失败成员自己结束了链
    /// （或不在链中）时才停止本次提交。
    pub(crate) fn add(&mut self, ring: &Ring, mut member: Member<Op>) -> (Option<Chain<Op>>, Flow) {
        // LINK_TIMEOUT 只对紧挨着它的前一个链成员有意义：不在链中，或前一个成员也是 LINK_TIMEOUT，
        // 都是 Linux 的 `-EINVAL`（缺失该校验时，孤立的 timeout 没有对象可约束，会被静默当成 no-op）。
        if matches!(member.step, Step::LinkTimeout(_)) {
            let after_member = self.open.as_ref().is_some_and(|chain| {
                !matches!(
                    chain.rest.last().unwrap_or(&chain.head).step,
                    Step::LinkTimeout(_)
                )
            });
            if !after_member {
                member.step = Step::Failed(-(errno::EINVAL as i32));
            }
        }
        let failed = matches!(member.step, Step::Failed(_));
        let continues = member.link != Link::None;
        let chain = match self.open.take() {
            Some(mut chain) => {
                if member.drain {
                    // 链中间的 DRAIN：链头 DRAIN，且链之后的下一条链也 DRAIN。
                    chain.drain = true;
                    ring.drain_next()
                        .store(true, core::sync::atomic::Ordering::Release);
                }
                if chain.rest.try_reserve(1).is_err() {
                    // 无法记录该成员：放弃整条链，成员得到 ENOMEM，并停止提交以免剩余成员被错组成新链。
                    fail_chain(ring, chain);
                    ring.complete(member.user_data, false, -(errno::ENOMEM as i32));
                    return (None, Flow::Stop);
                }
                chain.failed |= failed;
                chain.rest.push(member);
                chain
            }
            None => {
                let drain = member.drain
                    | ring
                        .drain_next()
                        .swap(false, core::sync::atomic::Ordering::AcqRel);
                Chain {
                    head: member,
                    rest: Vec::new(),
                    drain,
                    failed,
                }
            }
        };
        if continues {
            self.open = Some(chain);
            return (None, Flow::Continue);
        }
        if chain.failed {
            fail_chain(ring, chain);
            return (None, if failed { Flow::Stop } else { Flow::Continue });
        }
        (Some(chain), Flow::Continue)
    }

    /// 批末：仍未结束的链按现状结束（Linux `io_submit_state_end`）。
    pub(crate) fn end(&mut self, ring: &Ring) -> Option<Chain<Op>> {
        let chain = self.open.take()?;
        if chain.failed {
            fail_chain(ring, chain);
            return None;
        }
        Some(chain)
    }
}
