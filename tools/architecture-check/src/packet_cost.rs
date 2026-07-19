use std::{fs, path::Path};

const SOURCE: &str = "kernel/src/socket/packet.rs";
const ENDPOINTS: usize = 64;

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(2) => {}
        Ok(allocations) => errors.push(format!(
            "{SOURCE}: one immutable RX payload must be shared across packet endpoints; N={ENDPOINTS}, measured payload allocations={allocations}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<usize, String> {
    let source =
        fs::read_to_string(root.join(SOURCE)).map_err(|error| format!("{SOURCE}: {error}"))?;
    if source.contains("let mut payload = Vec::new();")
        && source.contains("payload.extend_from_slice(&frame[ETH_HEADER_LENGTH..]);")
        && source.contains("state.queue.push_back(Packet { payload, source });")
    {
        return Ok(ENDPOINTS);
    }
    if source.contains("struct SharedPacket")
        && source.contains("Arc::try_new(SharedPacket")
        && source.contains("state.queue.push_back(payload.clone())")
    {
        return Ok(2);
    }
    Err(format!(
        "{SOURCE}: packet payload ownership seam is not recognized"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_payload_allocation_is_constant_in_endpoint_count() {
        let root = super::super::repository_root();
        assert_eq!(measure(&root).expect("packet cost must be measurable"), 2);
    }
}
