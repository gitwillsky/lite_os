//! Management request for an uplink DHCP lease in the selected network domain.

use super::*;

const COMMAND_VERSION: u32 = 1;

/// Encode an operation against the actual interface name in `domain`, or the default domain.
/// # Errors
/// The domain or interface name is not valid.
pub fn encode(domain: Option<&str>, name: &[u8], action: DhcpAction) -> Result<Vec<u8>, Invalid> {
    validate(domain, name)?;
    let mut wire = Encoder::default();
    wire.u32(COMMAND_VERSION)
        .bytes(domain.unwrap_or_default().as_bytes())
        .bytes(name)
        .u8(action as u8);
    Ok(wire.0)
}

/// Decode the full versioned request and validate its domain and interface names.
/// # Errors
/// The version, names, action or message length is invalid.
pub fn decode(bytes: &[u8]) -> Result<(Option<&str>, &[u8], DhcpAction), Invalid> {
    let mut wire = Decoder::new(bytes);
    if wire.u32()? != COMMAND_VERSION {
        return Err(Invalid);
    }
    let domain = core::str::from_utf8(wire.bytes()?).map_err(|_| Invalid)?;
    let domain = (!domain.is_empty()).then_some(domain);
    let name = wire.bytes()?;
    let action = DhcpAction::decode(wire.u8()?)?;
    wire.finish()?;
    validate(domain, name)?;
    Ok((domain, name, action))
}

fn validate(domain: Option<&str>, name: &[u8]) -> Result<(), Invalid> {
    if domain.is_some_and(|name| !configuration::valid_routing_domain_name(name))
        || name.is_empty()
        || name.len() >= NAME_MAX
        || name.contains(&0)
    {
        return Err(Invalid);
    }
    Ok(())
}
