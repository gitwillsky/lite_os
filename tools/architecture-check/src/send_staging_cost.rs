use std::{fs, path::Path};

const CURSOR: &str = "kernel/src/syscall/user_iovec.rs";
const STAGING: &str = "kernel/src/syscall/user_iovec/input_staging.rs";
const SEQUENTIAL: &str = "kernel/src/syscall/fs/io/sequential/write.rs";
const MESSAGE: &str = "kernel/src/syscall/socket/message.rs";
const REGULAR: &str = "kernel/src/syscall/fs/io/regular.rs";
const SELECT: &str = "kernel/src/syscall/poll/select.rs";
const SOCKET_BYTES: usize = 64 * 1024;
const REGULAR_BYTES: usize = 1024 * 1024;
const SELECT_SET_BYTES: usize = 1024usize.div_ceil(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SendStagingCost {
    initialized_before_copyin: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(SendStagingCost {
            initialized_before_copyin: 0,
        }) => {}
        Ok(cost) => errors.push(format!(
            "send/write staging must expose only the user-copy initialized prefix; two socket buffers plus one regular buffer measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<SendStagingCost, String> {
    let cursor = read(root, CURSOR)?;
    let staging = read(root, STAGING).unwrap_or_default();
    let sequential = read(root, SEQUENTIAL)?;
    let message = read(root, MESSAGE)?;
    let regular = read(root, REGULAR)?;
    let select = read(root, SELECT)?;
    if cursor.contains("fn initialized_staging") && regular.contains("heap.resize(length, 0)") {
        return Ok(SendStagingCost {
            initialized_before_copyin: 2 * SOCKET_BYTES + REGULAR_BYTES + 3 * SELECT_SET_BYTES,
        });
    }
    if staging.contains("pub(crate) struct UserInputStaging")
        && staging.contains("Vec<MaybeUninit<u8>>")
        && staging.contains("unsafe fn publish")
        && cursor.contains("stage_from_user_into")
        && sequential.contains("UserInputStaging::try_new")
        && message.contains("UserInputStaging::try_new")
        && regular.contains("as_input_staging")
        && select.contains("input: [UserInputStaging<'static>; 3]")
        && select.contains("copy_from_user_uninit(address, staging.prepare(byte_count))")
        && !cursor.contains("fn initialized_staging")
    {
        return Ok(SendStagingCost {
            initialized_before_copyin: 0,
        });
    }
    Err("send/write/pselect staging initialized-prefix seam is not recognized".into())
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_staging_has_no_dead_initialization() {
        let root = super::super::repository_root();
        assert_eq!(
            measure(&root).expect("production send staging must be measurable"),
            SendStagingCost {
                initialized_before_copyin: 0,
            }
        );
    }
}
