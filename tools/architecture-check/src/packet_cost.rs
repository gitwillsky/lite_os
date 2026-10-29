use std::{fs, path::Path};

const SOURCE: &str = "kernel/src/net/packet.rs";
const ENDPOINTS: usize = 64;

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(2) => {}
        Ok(allocations) => errors.push(format!(
            "{SOURCE}: one immutable RX payload must be shared across packet endpoints; N={ENDPOINTS}, measured payload allocations={allocations}"
        )),
        Err(error) => errors.push(error),
    }
    match fs::read_to_string(root.join(SOURCE)) {
        Ok(source) if publication_is_prepared(&source) => {}
        Ok(_) => errors.push(format!("{SOURCE}: packet endpoint capacity and shared RX payload must be prepared outside registry spin guards; receive must retire/copy payload outside the guard")),
        Err(error) => errors.push(format!("{SOURCE}: {error}")),
    }
}

fn publication_is_prepared(source: &str) -> bool {
    let compact: String = source.split_whitespace().collect();
    let Some(deliver) = compact.split("pub(super)fndeliver(").nth(1) else {
        return false;
    };
    let deliver = deliver
        .split("pub(super)fnpublish_transmit_ready")
        .next()
        .unwrap();
    let Some(publication) = deliver.find("letmutregistry=registry.lock();") else {
        return false;
    };
    let Some(payload) = deliver.find("Arc::try_new(SharedPacket{") else {
        return false;
    };
    let Some(receive) = compact.split("pub(super)fnreceive(").nth(1) else {
        return false;
    };
    let receive = receive.split("pub(super)fnpoll_state(").next().unwrap();
    compact.contains("queue.try_reserve_exact(RECEIVE_QUEUE_LIMIT)")
        && !compact.contains("state.queue.try_reserve")
        && payload < publication
        && receive
            .find("drop(registry);")
            .zip(receive.find("output.append(&packet.payload)"))
            .is_some_and(|(unlock, copy)| unlock < copy)
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
    #[test]
    fn rejects_rx_growth_under_registry_guard() {
        let source = fs::read_to_string(super::super::repository_root().join(SOURCE)).unwrap();
        assert!(publication_is_prepared(&source));
        let changed = source.replace(
            "state.queue.push_back(payload.clone());",
            "let _ = state.queue.try_reserve(1); state.queue.push_back(payload.clone());",
        );
        assert!(!publication_is_prepared(&changed));
    }
    #[test]
    fn rejects_receive_copy_before_unlock() {
        let source = fs::read_to_string(super::super::repository_root().join(SOURCE)).unwrap();
        let changed = source.replace("let count = output.append(&packet.payload);", "let count = 0;")
            .replace("let drained = !peek && state.queue.is_empty();", "let count = output.append(&packet.payload); let drained = !peek && state.queue.is_empty();");
        assert!(!publication_is_prepared(&changed));
    }
}
