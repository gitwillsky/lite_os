use std::{fs, path::PathBuf};

const CPUS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WaitRegistryCost {
    independent_publication_contended_pairs: usize,
    maximum_parallel_publications: usize,
    readiness_callbacks_under_registry_lock: usize,
    maximum_registry_backend_lock_depth: usize,
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("architecture-check must live under tools/")
        .to_path_buf()
}

fn read(path: &str) -> String {
    fs::read_to_string(repository_root().join(path))
        .unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("missing `{signature}`"));
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
                    return &body[..=offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated `{signature}`")
}

fn callback_is_under_lock(body: &str, lock: &str, callback: &str) -> bool {
    let Some(lock) = body.find(lock) else {
        return false;
    };
    body[lock..].contains(callback)
}

fn measure() -> WaitRegistryCost {
    let registry = read("kernel/src/task/task_manager/wait_registry.rs");
    let task_manager = read("kernel/src/task/task_manager.rs");
    let pipe_wait = read("kernel/src/task/task_manager/pipe_wait.rs");
    let global_owner = registry.contains("static INDEXED_WAIT_QUEUE: IrqMutex<IndexedWaitQueue>");
    let sharded_owner = registry.contains("WAIT_SHARD_COUNT")
        && registry.contains("shards: [IrqMutex<WaitShard>; WAIT_SHARD_COUNT]")
        && !global_owner;

    let wait_for_poll = function_body(&task_manager, "pub(crate) fn wait_for_poll(");
    let wake_pipe_waiters = function_body(&pipe_wait, "fn wake_pipe_waiters(");
    let readiness_callbacks_under_registry_lock = usize::from(callback_is_under_lock(
        wait_for_poll,
        "INDEXED_WAIT_QUEUE.lock()",
        "ready()",
    )) + usize::from(callback_is_under_lock(
        wake_pipe_waiters,
        "INDEXED_WAIT_QUEUE.lock()",
        "pipe.poll_state(",
    ));

    if sharded_owner {
        WaitRegistryCost {
            independent_publication_contended_pairs: 0,
            maximum_parallel_publications: CPUS,
            readiness_callbacks_under_registry_lock,
            maximum_registry_backend_lock_depth: usize::from(
                readiness_callbacks_under_registry_lock != 0,
            ),
        }
    } else {
        WaitRegistryCost {
            independent_publication_contended_pairs: CPUS * (CPUS - 1) / 2,
            maximum_parallel_publications: 1,
            readiness_callbacks_under_registry_lock,
            // ppoll/epoll callback 可沿 registry -> epoll state -> backend source 嵌套。
            maximum_registry_backend_lock_depth: 3,
        }
    }
}

#[test]
fn independent_sources_do_not_share_one_owner_or_recheck_readiness_under_it() {
    let cost = measure();
    assert_eq!(
        cost,
        WaitRegistryCost {
            independent_publication_contended_pairs: 0,
            maximum_parallel_publications: CPUS,
            readiness_callbacks_under_registry_lock: 0,
            maximum_registry_backend_lock_depth: 0,
        },
        "8 CPUs use independent futex/pipe/deadline sources; measured {cost:?}"
    );
}
