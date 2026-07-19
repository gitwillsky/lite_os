use std::{fs, path::Path};

const EPOLL_OWNER_SOURCE: &str = "kernel/src/fs/epoll.rs";
const EPOLL_SYSCALL_SOURCE: &str = "kernel/src/syscall/epoll.rs";
const WAIT_KEY_SOURCE: &str = "kernel/src/syscall/poll/wait_keys.rs";
const PIPE_NOTIFY_SOURCE: &str = "kernel/src/task/task_manager/pipe_wait.rs";
const WAIT_PREPARATION_SOURCE: &str = "kernel/src/task/task_manager/wait_registry/preparation.rs";
const CONSOLE_NOTIFY_SOURCE: &str = "kernel/src/task/task_manager/console_wait.rs";
const UNIX_LIFECYCLE_SOURCE: &str = "kernel/src/socket/unix/lifecycle.rs";

const INTERESTS: usize = 128;
const SPURIOUS_WAKES: usize = 32;
const EPOLL_INSTANCES: usize = 16;
const FINAL_OFD_CLOSES: usize = 64;
const MEMBERSHIPS_PER_OFD: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EpollCost {
    pub(super) interest_clones: usize,
    pub(super) poll_visits: usize,
    pub(super) wait_key_reserve_attempts: usize,
    pub(super) wait_allocation_attempts: usize,
    pub(super) close_registry_visits: usize,
    pub(super) close_interest_visits: usize,
    pub(super) close_weak_upgrades: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(cost) if within_incremental_budget(cost) => {}
        Ok(cost) => errors.push(format!(
            "{EPOLL_OWNER_SOURCE}: epoll wait/close must use incremental ready and reverse membership owners; measured E={INTERESTS}, W={SPURIOUS_WAKES}, P={EPOLL_INSTANCES}, N={FINAL_OFD_CLOSES}: {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
    if let Err(error) = check_direct_pipe_wake_one(root) {
        errors.push(error);
    }
}

fn check_direct_pipe_wake_one(root: &Path) -> Result<(), String> {
    let preparation = read(root, WAIT_PREPARATION_SOURCE)?;
    let prepare_pipe = function_body(&preparation, "fn prepare_pipe(", WAIT_PREPARATION_SOURCE)?;
    if prepare_pipe.contains("exclusive: true") {
        Ok(())
    } else {
        Err(format!(
            "{WAIT_PREPARATION_SOURCE}: direct pipe waits must be exclusive so one readiness transition wakes one waiter"
        ))
    }
}

fn within_incremental_budget(cost: EpollCost) -> bool {
    cost.interest_clones <= INTERESTS
        && cost.poll_visits <= INTERESTS
        && cost.wait_key_reserve_attempts <= INTERESTS
        && cost.wait_allocation_attempts <= INTERESTS + EPOLL_INSTANCES
        && cost.close_registry_visits == 0
        && cost.close_interest_visits <= FINAL_OFD_CLOSES * MEMBERSHIPS_PER_OFD
        && cost.close_weak_upgrades == 0
}

pub(super) fn measure(root: &Path) -> Result<EpollCost, String> {
    let owner = read(root, EPOLL_OWNER_SOURCE)?;
    let syscall = read(root, EPOLL_SYSCALL_SOURCE)?;
    let wait_keys = read(root, WAIT_KEY_SOURCE)?;
    let pipe_notify = read(root, PIPE_NOTIFY_SOURCE)?;
    let console_notify = read(root, CONSOLE_NOTIFY_SOURCE)?;
    let unix_lifecycle = read(root, UNIX_LIFECYCLE_SOURCE)?;
    let evaluate = function_body(&syscall, "fn evaluate(", EPOLL_SYSCALL_SOURCE)?;
    let release = function_body(&owner, "pub(crate) fn release_file(", EPOLL_OWNER_SOURCE)?;

    let legacy_wait = evaluate.contains("epoll.snapshot()")
        && evaluate.contains("for interest in snapshot")
        && evaluate.contains("keys.add_interest(")
        && evaluate.contains("poll_events(")
        && wait_keys.contains("self.keys.try_reserve(1)");
    let legacy_close = release.contains("for entry in registry.iter()")
        && release.contains(".retain(|_, interest| !Arc::ptr_eq(&interest.ofd, closed))");
    if legacy_wait && legacy_close {
        let evaluations = SPURIOUS_WAKES + 1;
        return Ok(EpollCost {
            interest_clones: evaluations * INTERESTS,
            poll_visits: evaluations * INTERESTS,
            wait_key_reserve_attempts: evaluations * (INTERESTS + 1),
            wait_allocation_attempts: evaluations * (INTERESTS + 4),
            close_registry_visits: FINAL_OFD_CLOSES * EPOLL_INSTANCES * 2,
            close_interest_visits: FINAL_OFD_CLOSES * EPOLL_INSTANCES * INTERESTS,
            close_weak_upgrades: FINAL_OFD_CLOSES * EPOLL_INSTANCES,
        });
    }

    let incremental_wait = owner.contains("ready: FallibleMap<InterestKey, ()>")
        && owner.contains("source_nodes: [Option<SourceNode>; 2]")
        && owner.contains("ready_node: Option<ReadyNode>")
        && syscall.contains(".ready_snapshot(maximum)")
        && !evaluate.contains("keys.add_interest(")
        && !evaluate.contains("poll_events(")
        && pipe_notify.contains("Epoll::notify_pipe_source(pipe)")
        && console_notify.contains("Epoll::notify_console_source()")
        && unix_lifecycle.contains("client.notify();");
    let reverse_close = owner.contains("closed.epoll_memberships.take_first()")
        && release.contains("epoll.detach(membership.interest, closed)")
        && !release.contains("EPOLLS")
        && !release.contains("registry");
    if incremental_wait && reverse_close {
        // ADD 各做一次 initial readiness refresh；false wake 只重建单个
        // epoll notification key/guard，不 clone/poll E interests。final close 只消费
        // OFD reverse memberships，不访问全局 weak registry。
        let evaluations = SPURIOUS_WAKES + 1;
        return Ok(EpollCost {
            interest_clones: 0,
            poll_visits: INTERESTS,
            wait_key_reserve_attempts: evaluations,
            wait_allocation_attempts: evaluations * 4,
            close_registry_visits: 0,
            close_interest_visits: FINAL_OFD_CLOSES * MEMBERSHIPS_PER_OFD,
            close_weak_upgrades: 0,
        });
    }

    Err(format!(
        "{EPOLL_OWNER_SOURCE}: production epoll owner seam is not recognized by the complexity gate"
    ))
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

fn function_body<'a>(source: &'a str, signature: &str, path: &str) -> Result<&'a str, String> {
    let start = source
        .find(signature)
        .ok_or_else(|| format!("{path}: missing `{signature}`"))?;
    let body = &source[start..];
    let mut depth = 0usize;
    let mut opened = false;
    for (offset, byte) in body.bytes().enumerate() {
        match byte {
            b'{' => {
                opened = true;
                depth += 1;
            }
            b'}' if opened => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&body[..=offset]);
                }
            }
            _ => {}
        }
    }
    Err(format!("{path}: unterminated `{signature}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spurious_waits_and_final_closes_have_incremental_cost() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production epoll cost must be measurable");
        assert!(
            within_incremental_budget(cost),
            "E={INTERESTS}, W={SPURIOUS_WAKES}, P={EPOLL_INSTANCES}, N={FINAL_OFD_CLOSES}, measured {cost:?}"
        );
    }

    #[test]
    fn direct_pipe_waits_are_wake_one() {
        let root = super::super::repository_root();
        check_direct_pipe_wake_one(&root)
            .expect("direct pipe wait must not cause a thundering herd");
    }
}
