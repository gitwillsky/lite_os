use std::{fs, path::Path};

const LOG_SOURCE: &str = "kernel/src/log.rs";
const CONSOLE_SOURCE: &str = "kernel/src/platform/qemu_virt/riscv64/console.rs";
const FIRMWARE_SOURCE: &str = "kernel/src/platform/qemu_virt/riscv64/firmware.rs";
const LINE_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogCost {
    sbi_calls: usize,
    filtered_locks: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(LogCost {
            sbi_calls: 1,
            filtered_locks: 0,
        }) => {}
        Ok(cost) => errors.push(format!(
            "{CONSOLE_SOURCE}: logging must batch DBCN writes and reject disabled levels before the IRQ lock; B={LINE_BYTES}, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<LogCost, String> {
    let log = read(root, LOG_SOURCE)?;
    let console = read(root, CONSOLE_SOURCE)?;
    let firmware = read(root, FIRMWARE_SOURCE)?;
    if console.contains("for byte in s.bytes()")
        && console.contains("debug_console_write(byte)")
        && log.contains("LOGGER.lock().log(level, module, args)")
        && !log.contains("pub(crate) fn enabled(level: LogLevel)")
    {
        return Ok(LogCost {
            sbi_calls: LINE_BYTES,
            filtered_locks: 1,
        });
    }
    if console.contains("const CONSOLE_BATCH_BYTES: usize = 256")
        && console.contains("debug_console_write_bytes")
        && firmware.contains("FID_CONSOLE_WRITE")
        && log.contains("pub(crate) fn enabled(level: LogLevel)")
        && log.contains("if $crate::log::enabled(")
    {
        return Ok(LogCost {
            sbi_calls: LINE_BYTES.div_ceil(256),
            filtered_locks: 0,
        });
    }
    Err(format!(
        "{LOG_SOURCE}: log filtering/batch seam is not recognized"
    ))
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_log_line_uses_one_sbi_and_filtered_debug_uses_no_lock() {
        let root = super::super::repository_root();
        assert_eq!(
            measure(&root).expect("production log cost must be measurable"),
            LogCost {
                sbi_calls: 1,
                filtered_locks: 0
            }
        );
    }
}
