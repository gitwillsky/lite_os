use std::{fs, path::Path};

const QUEUE_SOURCE: &str = "kernel/src/task/task_manager/timer_queue.rs";
const TRANSACTION_SOURCE: &str = "kernel/src/task/task_manager/timer_queue/transaction.rs";
const LOOP_SOURCE: &str = "kernel/src/task/task_manager/timer_queue/transaction_loop.rs";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TimerTransactionCost {
    pub(super) duplicated_skeletons: usize,
    pub(super) executor_seams: usize,
    pub(super) explicit_policies: usize,
    pub(super) final_recheck_sites: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(cost) if within_budget(cost) => {}
        Ok(cost) => errors.push(format!(
            "{QUEUE_SOURCE}: timer mutation must use one prepare/final-lifecycle-recheck/commit executor for three explicit policies; measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn within_budget(cost: TimerTransactionCost) -> bool {
    cost == (TimerTransactionCost {
        duplicated_skeletons: 0,
        executor_seams: 1,
        explicit_policies: 3,
        final_recheck_sites: 1,
    })
}

pub(super) fn measure(root: &Path) -> Result<TimerTransactionCost, String> {
    let queue = read(root, QUEUE_SOURCE)?;
    let transaction = read(root, TRANSACTION_SOURCE).unwrap_or_default();
    let transaction_loop = read(root, LOOP_SOURCE).unwrap_or_default();
    let mutations = [
        function_body(&queue, "pub(crate) fn set_real_timer(")?,
        function_body(&queue, "pub(crate) fn create_posix_timer(")?,
        function_body(&queue, "pub(crate) fn set_posix_timer(")?,
    ];
    let duplicated_skeletons = mutations
        .iter()
        .filter(|body| {
            body.contains("loop {")
                && body.contains("::prepare(")
                && body.contains("graph.lock()")
                && body.contains("timers.lock()")
        })
        .count();
    let final_recheck_sites =
        duplicated_skeletons + transaction.matches("policy.validate(&graph)").count();
    let explicit_policies = ["ItimerReal", "PosixCreate", "PosixReplace"]
        .iter()
        .filter(|variant| transaction.contains(**variant))
        .count();
    let executor_seams = transaction.matches("fn execute_timer_transaction").count();

    if executor_seams == 1 && !safe_publication_order(&transaction, &transaction_loop) {
        return Err(format!(
            "{TRANSACTION_SOURCE}: prepare must run after the planning timer lock is dropped and before final graph→timer commit locks"
        ));
    }
    Ok(TimerTransactionCost {
        duplicated_skeletons,
        executor_seams,
        explicit_policies,
        final_recheck_sites,
    })
}

fn safe_publication_order(transaction: &str, transaction_loop: &str) -> bool {
    let loop_positions = [
        "let plan = plan()?",
        "prepare(plan, reusable.take())",
        "final_commit(prepared)",
    ]
    .map(|needle| transaction_loop.find(needle));
    let adapter_positions = ["policy.validate(&graph)", "commit(&mut timers, prepared)"]
        .map(|needle| transaction.find(needle));
    matches!(loop_positions, [Some(plan), Some(prepare), Some(commit)]
        if plan < prepare && prepare < commit)
        && matches!(adapter_positions, [Some(validate), Some(commit)] if validate < commit)
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

fn function_body<'a>(source: &'a str, signature: &str) -> Result<&'a str, String> {
    let start = source
        .find(signature)
        .ok_or_else(|| format!("{QUEUE_SOURCE}: missing {signature}"))?;
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
    Err(format!("{QUEUE_SOURCE}: unterminated {signature}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_uses_one_timer_transaction_skeleton() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("timer transaction cost must be measurable");
        assert!(within_budget(cost), "measured {cost:?}");
    }
}
