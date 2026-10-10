use std::{fs, path::Path};

const QUEUE_SOURCE: &str = "kernel/src/task/process_table/timer_queue.rs";
const TRANSACTION_SOURCE: &str = "kernel/src/task/process_table/timer_queue/transaction.rs";
const LOOP_SOURCE: &str = "kernel/src/task/process_table/timer_queue/transaction_loop.rs";

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
    if !transaction.contains("&mut Prepared")
        || !transaction.contains("let outcome = {")
        || !transaction.contains("None => TimerTransactionCommit::Retry(Some(prepared))")
        || !transaction.contains("}?;\n            Ok(match outcome")
    {
        return Err(format!(
            "{TRANSACTION_SOURCE}: final commit must only borrow storage; retry and Drop belong outside guards"
        ));
    }
    let posix_schedule = read(
        root,
        "kernel/src/task/process_table/timer_queue/posix_schedule.rs",
    )?;
    let timer_file = read(
        root,
        "kernel/src/task/process_table/timer_queue/timer_file.rs",
    )?;
    let kernel_wait = read(
        root,
        "kernel/src/task/process_table/timer_queue/kernel_wait.rs",
    )?;
    for (path, body) in [
        (QUEUE_SOURCE, queue.as_str()),
        ("timer_file", timer_file.as_str()),
        ("kernel_wait", kernel_wait.as_str()),
        ("posix_schedule", posix_schedule.as_str()),
    ] {
        if body.contains(".remove(") {
            return Err(format!(
                "{path}: timer mutation/expiry must retire actual AVL nodes outside owner, not only removed values"
            ));
        }
    }
    if !queue.contains("retired.real.commit_vacant")
        || !queue.contains("retired.posix.commit_vacant")
        || !queue.contains("Some((previous, retired))")
        || !posix_schedule.contains("Ok(Some((previous, retired)))")
        || !posix_schedule
            .contains("PosixSchedule::Notification { deadline, .. } => Some(deadline)")
        || !posix_schedule.contains("drop(retired);")
        || !timer_file.contains("&mut deadline_node")
        || !timer_file.contains("drop(expirations);\n            drop(retired);")
        || !kernel_wait.contains("TimerExpirationRetirement::from_deadline(Some(entry))")
    {
        return Err(format!(
            "{QUEUE_SOURCE}: all actual record/deadline nodes need owned lock-outside retirement, including stale and silent expiry"
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
    let adapter_positions = [
        "policy.validate(&graph)",
        "commit(&mut timers, &mut prepared)",
    ]
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
    #[test]
    fn owner_proof_covers_borrowed_preparation_and_lock_outside_node_retirement() {
        let root = super::super::repository_root();
        let transaction = read(&root, TRANSACTION_SOURCE).unwrap();
        let loop_source = read(&root, LOOP_SOURCE).unwrap();
        assert!(safe_publication_order(&transaction, &loop_source));
        assert!(!safe_publication_order(
            &transaction.replace(
                "commit(&mut timers, &mut prepared)",
                "consume_storage_under_owner(prepared)"
            ),
            &loop_source
        ));
        assert!(transaction.contains("None => TimerTransactionCommit::Retry(Some(prepared))"));
    }
}
