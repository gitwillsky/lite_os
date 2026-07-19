use std::{fs, path::Path};

const SEQUENTIAL: &str = "kernel/src/syscall/fs/io/sequential.rs";
const SEQUENTIAL_READ: &str = "kernel/src/syscall/fs/io/sequential/read.rs";
const MESSAGE: &str = "kernel/src/syscall/socket/message.rs";
const REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StagingCost {
    initialized_before_receive: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(StagingCost {
            initialized_before_receive: 0,
        }) => {}
        Ok(cost) => errors.push(format!(
            "receive staging must reserve capacity without zero-filling bytes that every backend overwrites; two {REQUEST_BYTES}-byte buffers measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<StagingCost, String> {
    let sequential = read(root, SEQUENTIAL)?;
    let sequential_read = read(root, SEQUENTIAL_READ)?;
    let message = read(root, MESSAGE)?;
    let legacy = usize::from(
        sequential.contains("bytes.resize(length, 0)")
            && sequential.contains("fn buffer(length: usize)"),
    ) + usize::from(
        message.contains("bytes.resize(length, 0)")
            && message.contains("fn message_buffer(length: usize)"),
    );
    if legacy != 0 {
        return Ok(StagingCost {
            initialized_before_receive: legacy * REQUEST_BYTES,
        });
    }
    if sequential_read.contains("ReceiveBuffer::try_new")
        && message.contains("ReceiveBuffer::try_new")
        && !sequential.contains("fn buffer(length: usize)")
        && !message.contains("fn message_buffer(length: usize)")
    {
        return Ok(StagingCost {
            initialized_before_receive: 0,
        });
    }
    Err("receive staging ownership seam is not recognized".into())
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_staging_does_not_initialize_dead_bytes() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production receive staging must be measurable");
        assert_eq!(
            cost.initialized_before_receive, 0,
            "two B={REQUEST_BYTES} receive buffers measured {cost:?}"
        );
    }
}
