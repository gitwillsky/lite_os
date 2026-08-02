//! VirtIO Console control-message wire validation.

const SPICE_PORT_NAME: &[u8] = b"com.redhat.spice.0";

/// Maps a VirtIO Console port identity to its standard receive/transmit queues.
pub(super) fn data_queue_indices(port_id: u32) -> Option<(u32, u32)> {
    let receive = if port_id == 0 {
        0
    } else {
        port_id.checked_mul(2)?.checked_add(2)?
    };
    Some((receive, receive.checked_add(1)?))
}

/// Matches QEMU's NUL-terminated `PORT_NAME` payload exactly.
///
/// Including the terminator in the name comparison leaves the selected port
/// permanently disconnected and surfaces as `POLLERR | POLLHUP` to userland.
pub(super) fn is_spice_port_name(payload: &[u8]) -> bool {
    payload.strip_suffix(&[0]) == Some(SPICE_PORT_NAME)
}

#[cfg(test)]
mod tests {
    use super::{data_queue_indices, is_spice_port_name};

    #[test]
    fn accepts_only_the_nul_terminated_spice_port_name() {
        assert!(is_spice_port_name(b"com.redhat.spice.0\0"));
        assert!(!is_spice_port_name(b"com.redhat.spice.0"));
        assert!(!is_spice_port_name(b"com.redhat.spice.00\0"));
        assert!(!is_spice_port_name(b"com.redhat.spice.0\0\0"));
    }

    #[test]
    fn maps_each_port_identity_to_its_owned_queue_pair() {
        assert_eq!(data_queue_indices(0), Some((0, 1)));
        assert_eq!(data_queue_indices(1), Some((4, 5)));
        assert_eq!(data_queue_indices(2), Some((6, 7)));
        assert_eq!(data_queue_indices(u32::MAX), None);
    }
}
