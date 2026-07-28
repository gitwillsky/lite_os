//! VirtIO Console control-message wire validation.

const SPICE_PORT_NAME: &[u8] = b"com.redhat.spice.0";

/// Matches QEMU's NUL-terminated `PORT_NAME` payload exactly.
///
/// Including the terminator in the name comparison leaves the selected port
/// permanently disconnected and surfaces as `POLLERR | POLLHUP` to userland.
pub(super) fn is_spice_port_name(payload: &[u8]) -> bool {
    payload.strip_suffix(&[0]) == Some(SPICE_PORT_NAME)
}

#[cfg(test)]
mod tests {
    use super::is_spice_port_name;

    #[test]
    fn accepts_only_the_nul_terminated_spice_port_name() {
        assert!(is_spice_port_name(b"com.redhat.spice.0\0"));
        assert!(!is_spice_port_name(b"com.redhat.spice.0"));
        assert!(!is_spice_port_name(b"com.redhat.spice.00\0"));
        assert!(!is_spice_port_name(b"com.redhat.spice.0\0\0"));
    }
}
