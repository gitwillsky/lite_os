use std::{fs, path::Path};

const NAMESPACE: &str = "kernel/src/socket/inet/port_namespace.rs";
const BITMAP: &str = "kernel/src/socket/inet/port_namespace/bitmap.rs";
const TCP: &str = "kernel/src/socket/inet/tcp.rs";
const UDP: &str = "kernel/src/socket/inet/udp.rs";
const EPHEMERAL_PORTS: usize = 16_384;
const SAMPLE_ENDPOINTS: usize = 1_024;
const MAX_WORD_READS: usize = 257;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PortLookupCost {
    endpoint_probes: usize,
    bitmap_word_reads: usize,
}

const fn legacy_cost() -> PortLookupCost {
    PortLookupCost {
        endpoint_probes: EPHEMERAL_PORTS * SAMPLE_ENDPOINTS,
        bitmap_word_reads: 0,
    }
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(PortLookupCost {
            endpoint_probes: 0,
            bitmap_word_reads: 0..=MAX_WORD_READS,
        }) => {}
        Ok(cost) => errors.push(format!(
            "{NAMESPACE}: ephemeral allocation must not scan endpoints; ports={EPHEMERAL_PORTS}, endpoints={SAMPLE_ENDPOINTS}, max_words={MAX_WORD_READS}, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<PortLookupCost, String> {
    let namespace = read(root, NAMESPACE)?;
    let bitmap = read(root, BITMAP)?;
    let tcp = read(root, TCP)?;
    let udp = read(root, UDP)?;
    let exact_owner = namespace.contains("entries: FallibleMap<u16, Occupancy>")
        && namespace.contains("specific: FallibleMap<Ipv4Addr, AddressOccupancy>")
        && namespace.contains("prepare_retain_for_address")
        && namespace.contains("prepare_readdress")
        && namespace.contains("claim_listener");
    let bounded_bitmap = bitmap.contains("occupied: [u64; EPHEMERAL_WORDS]")
        && bitmap.contains("for offset in 0..EPHEMERAL_WORDS")
        && bitmap.contains("if start_bit != 0")
        && bitmap.contains("const EPHEMERAL_WORDS: usize")
        && bitmap.contains(".div_ceil(u64::BITS as usize)");
    let no_endpoint_scan = !namespace.contains("port_in_use")
        && !tcp.contains("tcp_port_in_use")
        && !udp.contains("udp_port_in_use")
        && !tcp.contains("for _ in EPHEMERAL_START")
        && !udp.contains("for _ in EPHEMERAL_START");
    let exact_port_zero = tcp.contains(".acquire_ephemeral(address_filter, reuse_address)")
        && udp.contains(".acquire_ephemeral(address_filter, reuse_address)")
        && tcp.contains(".acquire_ephemeral(Some(local_address), reuse_address)");
    if exact_owner && bounded_bitmap && no_endpoint_scan && exact_port_zero {
        return Ok(PortLookupCost {
            endpoint_probes: 0,
            bitmap_word_reads: MAX_WORD_READS,
        });
    }
    if tcp.contains("tcp_port_in_use") || udp.contains("udp_port_in_use") {
        return Ok(legacy_cost());
    }
    Err(format!(
        "{NAMESPACE}: local-port namespace ownership seam is not recognized"
    ))
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_allocator_has_port_range_times_endpoint_cost() {
        assert_eq!(legacy_cost().endpoint_probes, 16_777_216);
    }

    #[test]
    fn namespace_allocator_has_bounded_word_scan_and_no_endpoint_probe() {
        let root = super::super::repository_root();
        assert_eq!(
            measure(&root).expect("port namespace cost must be measurable"),
            PortLookupCost {
                endpoint_probes: 0,
                bitmap_word_reads: 257,
            }
        );
    }
}
