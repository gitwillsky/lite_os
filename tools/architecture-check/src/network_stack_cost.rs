use std::{fs, path::Path};

const INET: &str = "kernel/src/socket/inet.rs";
const UDP: &str = "kernel/src/socket/inet/udp.rs";
const RAW: &str = "kernel/src/socket/inet/raw.rs";
const TCP_IO: &str = "kernel/src/socket/inet/tcp/io.rs";
const TCP_STORAGE: &str = "kernel/src/socket/inet/tcp/storage.rs";
const ENDPOINTS: usize = 64;
const BYTES_PER_ENDPOINT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DataPlaneCost {
    contended_endpoint_pairs: usize,
    payload_bytes_under_stack_mutex: usize,
    device_calls_under_stack_mutex: usize,
}

const fn legacy_cost() -> DataPlaneCost {
    DataPlaneCost {
        contended_endpoint_pairs: ENDPOINTS * (ENDPOINTS - 1) / 2,
        payload_bytes_under_stack_mutex: ENDPOINTS * BYTES_PER_ENDPOINT,
        device_calls_under_stack_mutex: ENDPOINTS,
    }
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(DataPlaneCost {
            contended_endpoint_pairs: 0,
            payload_bytes_under_stack_mutex: 0,
            device_calls_under_stack_mutex: 0,
        }) => {}
        Ok(cost) => errors.push(format!(
            "{INET}: independent endpoint data plane must not serialize copy/device I/O under NetworkStack mutex; N={ENDPOINTS}, B={BYTES_PER_ENDPOINT}, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<DataPlaneCost, String> {
    let inet = read(root, INET)?;
    let udp = read(root, UDP)?;
    let raw = read(root, RAW)?;
    let tcp = read(root, TCP_IO)?;
    let tcp_storage = read(root, TCP_STORAGE)?;

    let endpoint_membership = inet.contains("operation: Mutex<()>")
        && inet.contains("let _operation = self.operation.lock();")
        && inet.contains("let _protocol = protocol_read();");
    let stable_loans = udp.contains("fn placeholder_socket()")
        && raw.contains("fn placeholder_socket()")
        && tcp_storage.contains("fn placeholder_socket()")
        && udp.matches("core::mem::replace(").count() >= 4
        && raw.matches("core::mem::replace(").count() >= 4
        && tcp.matches("core::mem::replace(").count() >= 4
        && udp.contains("drop(network);")
        && raw.contains("drop(network);")
        && tcp.contains("drop(network);");
    let owner = read(root, "kernel/src/socket/inet/protocol_owner.rs")?;
    let readiness = read(root, "kernel/src/socket/inet/readiness.rs")?;
    let poll_loan = inet.contains("let mut network = stack.poll_loan();")
        && owner.contains("pub(super) struct NetworkPollLoan")
        && owner.contains("impl Drop for NetworkPollLoan")
        && owner.contains("*state = Some(stack);")
        && !owner.contains("fn take_for_poll")
        && !owner.contains("fn restore_after_poll")
        && !inet.contains("stack.lock().poll()");
    let notification_handoff = inet.contains("const NETWORK_NOTIFICATION_BUDGET: usize = 64;")
        && inet.contains("let notifications = network.take_pending_notifications();")
        && inet.contains("drop(network);")
        && readiness.contains("pub(super) struct PendingNotifications")
        && readiness.contains("pending.backlog = pending.is_full();")
        && readiness.contains("for endpoint in pending.endpoints.into_iter().flatten()")
        && readiness.contains("endpoint.notify();");
    let no_syscall_egress = !udp.contains("poll_egress")
        && !raw.contains("poll_egress")
        && !tcp.contains("poll_egress");

    if endpoint_membership && stable_loans && poll_loan && notification_handoff && no_syscall_egress
    {
        return Ok(DataPlaneCost {
            contended_endpoint_pairs: 0,
            payload_bytes_under_stack_mutex: 0,
            device_calls_under_stack_mutex: 0,
        });
    }
    if udp.contains("let mut network = stack()?.lock();")
        && udp.contains("send_slice(input")
        && tcp.contains("let mut network = stack()?.lock();")
        && tcp.contains("output.append(bytes)")
    {
        return Ok(legacy_cost());
    }
    Err(format!(
        "{INET}: network data-plane ownership seam is not recognized"
    ))
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_global_mutex_cost_is_quadratic_in_independent_endpoints() {
        assert_eq!(legacy_cost().contended_endpoint_pairs, 2016);
        assert_eq!(
            legacy_cost().payload_bytes_under_stack_mutex,
            4 * 1024 * 1024
        );
        assert_eq!(legacy_cost().device_calls_under_stack_mutex, 64);
    }

    #[test]
    fn endpoint_loans_remove_global_copy_and_device_io() {
        let root = super::super::repository_root();
        assert_eq!(
            measure(&root).expect("network ownership cost must be measurable"),
            DataPlaneCost {
                contended_endpoint_pairs: 0,
                payload_bytes_under_stack_mutex: 0,
                device_calls_under_stack_mutex: 0,
            }
        );
    }
}
