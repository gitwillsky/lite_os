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
    skip_success: bool,
    link: Link,
    kind: Kind,
}

/// 链停在一个需要等待的成员上：正在等待的 op、它的元数据与尚未 issue 的成员。
struct Waiting {
    wait: Wait,
    meta: Meta,
    rest: Members<Op>,
}

/// 链从哪里开始被 future 驱动。
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
fn advance(ring: &Arc<Ring>, task: &Weak<TaskControlBlock>, rest: &mut Members<Op>) -> Option<(Wait, Meta)> {
    while let Some(member) = rest.next() {
        let Member {
            user_data,
            skip_success,
            link,
            step,
            ..
        } = member;
        let mut meta = Meta {
            user_data,
            skip_success,
            link,
            kind: Kind::Io,
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
            (Step::Op(op), Some(task)) => match op.issue(&task, ring) {
                Issue::Done(outcome) => outcome,
                Issue::Wait(wait) => {
                    meta.kind = wait.kind();
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
        waiter.begin(meta.user_data, meta.kind);
        let outcome = drive(wait, ring.clone(), task.clone(), waiter.clone()).await;
        waiter.end();
        if !finish(&ring, &meta, outcome, &mut rest) {
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
        abandon(ring, start.take().expect("spawn failed before building the future"));
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
            cancel_with(ring, rest, -(errno::ENOMEM as i32));
        }
    }
}
