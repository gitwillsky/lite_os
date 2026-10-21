//! 请求链的执行：成员按提交顺序 issue，遇到需要等待的成员就把整条链的剩余部分交给 worker 的一个 future。
//!
//! 一条链（单个 SQE 是长度为 1 的链）同一时刻至多有一个成员在等待，所以整条链只需要一个 future、
//! 一个 [`Waiter`] 和一个 [`Unit`]（DRAIN 顺序的占位）。同步完成的成员在当前上下文里直接继续，
//! 不分配 future；这与成员在提交者还是在 worker 线程里执行无关。

use alloc::sync::{Arc, Weak};
use syscall_abi::errno;

use super::{
    exec::{Issue, Op, Outcome, Wait, drive},
    wait::{Kind, Waiter},
    worker,
};
use crate::{
    io_uring::{Chain, Link, Member, Members, Ring, Step, Unit, cancel_rest},
    task::TaskControlBlock,
};

/// 成员完成时需要的元数据；成员的 `Step` 已被消费。
struct Meta {
    user_data: u64,
    opcode: u8,
    skip_success: bool,
    link: Link,
    kind: Kind,
    /// 约束这个成员的 `LINK_TIMEOUT`：`(它的 user_data, deadline)`。成员需要等待时才存在。
    link_timer: Option<(u64, u64)>,
}

/// 链停在一个需要等待的成员上：正在等待的 op、它的元数据与尚未 issue 的成员。
struct Waiting {
    wait: Wait,
    meta: Meta,
    rest: Members<Op>,
}

/// 链从哪里开始被 future 驱动。
#[allow(
    clippy::large_enum_variant,
    reason = "Start is moved once into the boxed chain future; boxing a variant would add an allocation per request"
)]
enum Start {
    /// 前面的 DRAIN 还没完成：先等待被接纳。
    Deferred(Members<Op>),
    Waiting(Waiting),
}

/// 发布一个成员的 CQE 并决定链是否继续；失败的 `IOSQE_IO_LINK` 成员取消其后全部成员。
///
/// 失败使 `IOSQE_CQE_SKIP_SUCCESS` 失效（失败总是可见），且失败成员自己带 skip 时其后被取消的成员
/// 也不发布 CQE（Linux `REQ_F_SKIP_LINK_CQES`）。
fn finish(ring: &Ring, meta: &Meta, outcome: Outcome, rest: &mut Members<Op>) -> bool {
    let skip = meta.skip_success && !outcome.fail;
    match meta.kind {
        Kind::Timeout => ring.complete_timeout(meta.user_data, skip, outcome.res),
        _ => ring.complete(meta.user_data, skip, outcome.res),
    }
    if outcome.fail && meta.link == Link::Soft {
        cancel_rest(ring, rest, meta.skip_success);
        return false;
    }
    true
}

/// issue 成员直到某个成员需要等待（返回它）或链结束（`None`）。
fn advance(
    ring: &Arc<Ring>,
    task: &Weak<TaskControlBlock>,
    rest: &mut Members<Op>,
) -> Option<(Wait, Meta)> {
    while let Some(member) = rest.next() {
        let Member {
            user_data,
            opcode,
            skip_success,
            link,
            step,
            ..
        } = member;
        let mut meta = Meta {
            user_data,
            opcode,
            skip_success,
            link,
            kind: Kind::Io,
            link_timer: None,
        };
        let outcome = match (step, task.upgrade()) {
            (Step::Failed(_), _) => {
                unreachable!("a chain with a failed member is completed during assembly")
            }
            // 提交线程已退出：没有 fd 表和地址空间可用，成员以 -ECANCELED 结束。
            (_, None) => Outcome::canceled(),
            (Step::Nop(nop), Some(task)) => Outcome::of(
                nop.result(|fd| usize::try_from(fd).is_ok_and(|fd| task.fd_get(fd).is_some()))
                    as isize,
            ),
            (Step::Op(op) | Step::LinkTimeout(op), Some(task)) => match op.issue(&task, ring) {
                Issue::Done(outcome) => outcome,
                Issue::Wait(wait) => {
                    meta.kind = wait.kind();
                    // 成员开始等待：紧随其后的 LINK_TIMEOUT 此刻开始计时（Linux 在 issue 时装定时器）。
                    meta.link_timer = rest.next_if_link_timeout().and_then(|timeout| {
                        let Step::LinkTimeout(op) = timeout.step else {
                            return None;
                        };
                        let deadline = op.link_deadline(crate::timer::get_time_ns())?;
                        Some((timeout.user_data, deadline))
                    });
                    return Some((wait, meta));
                }
            },
        };
        if !finish(ring, &meta, outcome, rest) {
            return None;
        }
    }
    None
}

/// 驱动一条挂起的链直到全部成员完成。丢弃它（取消）会丢弃 `unit`，释放 DRAIN 占位。
async fn run(
    start: Start,
    ring: Arc<Ring>,
    task: Weak<TaskControlBlock>,
    waiter: Waiter,
    unit: Unit,
) {
    let Waiting {
        mut wait,
        mut meta,
        mut rest,
    } = match start {
        Start::Waiting(waiting) => waiting,
        Start::Deferred(mut members) => {
            while !ring.may_start(unit.sequence()) {
                waiter.admission(unit.sequence()).await;
            }
            match advance(&ring, &task, &mut members) {
                Some((wait, meta)) => Waiting {
                    wait,
                    meta,
                    rest: members,
                },
                None => return,
            }
        }
    };
    loop {
        waiter.begin(meta.user_data, meta.kind, meta.opcode, wait.file());
        if let Some((_, deadline)) = meta.link_timer {
            waiter.arm_link_timeout(deadline);
        }
        let outcome = drive(wait, ring.clone(), task.clone(), waiter.clone()).await;
        waiter.end();
        let timed_out = waiter.disarm_link_timeout();
        let keep_going = finish(&ring, &meta, outcome, &mut rest);
        // timeout 自己的 CQE：定时器先到并取消了成员为 `-ETIME`，成员先完成（或因别的原因结束）为 `-ECANCELED`。
        // 它不属于链失败：被它约束的成员的结果已经决定了后继是否被取消。
        if let Some((user_data, _)) = meta.link_timer {
            let res = if timed_out {
                errno::ETIME
            } else {
                errno::ECANCELED
            };
            ring.complete(user_data, false, -(res as i32));
        }
        if !keep_going {
            return;
        }
        match advance(&ring, &task, &mut rest) {
            Some((next_wait, next_meta)) => {
                wait = next_wait;
                meta = next_meta;
            }
            None => return,
        }
    }
}

/// 开始执行一条未失败的链（见 [`crate::io_uring::Submitter::launch`]）。
///
/// 1. 接纳：分配 DRAIN 序号；失败以 `-ENOMEM` 完成全部成员。
/// 2. 能开始就在当前上下文里 issue 成员，直到某个成员需要等待；否则整条链等待被接纳。
/// 3. 仍有未完成部分时包成一个 future，先 poll 一次再交给 worker；任何分配失败都以 `-ENOMEM`
///    完成尚未完成的成员（`spawn` 在调用 `build` 之前完成全部分配，所以失败时 `start` 仍在手里）。
pub(super) fn launch(task: &Arc<TaskControlBlock>, ring: &Arc<Ring>, chain: Chain<Op>) {
    let task = Arc::downgrade(task);
    let drain = chain.drain();
    let mut members = chain.into_members();
    let Ok(unit) = ring.admit(drain) else {
        cancel_with(ring, members, -(errno::ENOMEM as i32));
        return;
    };
    let start = if ring.may_start(unit.sequence()) {
        match advance(ring, &task, &mut members) {
            Some((wait, meta)) => Start::Waiting(Waiting {
                wait,
                meta,
                rest: members,
            }),
            None => return,
        }
    } else {
        Start::Deferred(members)
    };
    let Ok(waiter) = Waiter::new(ring) else {
        abandon(ring, start);
        return;
    };
    let mut start = Some(start);
    let spawned = worker::spawn(waiter.clone(), || {
        run(
            start.take().expect("spawn builds the future at most once"),
            ring.clone(),
            task,
            waiter,
            unit,
        )
    });
    if spawned.is_err() {
        abandon(
            ring,
            start
                .take()
                .expect("spawn failed before building the future"),
        );
    }
}

fn cancel_with(ring: &Ring, members: Members<Op>, res: i32) {
    for member in members {
        ring.complete(member.user_data, false, res);
    }
}

/// 无法为链创建 future：以 `-ENOMEM` 完成正在等待的成员和全部后继。
fn abandon(ring: &Ring, start: Start) {
    match start {
        Start::Deferred(members) => cancel_with(ring, members, -(errno::ENOMEM as i32)),
        Start::Waiting(Waiting { meta, rest, .. }) => {
            ring.complete(meta.user_data, false, -(errno::ENOMEM as i32));
            if let Some((user_data, _)) = meta.link_timer {
                ring.complete(user_data, false, -(errno::ECANCELED as i32));
            }
            cancel_with(ring, rest, -(errno::ENOMEM as i32));
        }
    }
}
