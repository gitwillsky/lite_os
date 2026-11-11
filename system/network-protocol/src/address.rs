//! 网络地址的原生控制编码；双族 endpoint 保持标准 SocketAddr 语义，Linux sockaddr 属于 linuxd。
use super::{InetAddress, PacketAddress, Reader, Status, Writer, fixed, native_abi};
use core::net::Ipv4Addr;

pub(super) fn put_ipv4(writer: &mut Writer, address: Ipv4Addr) {
    writer.u32(u32::from_be_bytes(address.octets()));
}

pub(super) fn get_ipv4(reader: &mut Reader<'_>) -> Result<Ipv4Addr, Status> {
    Ok(Ipv4Addr::from(reader.u32()?.to_be_bytes()))
}

pub(super) fn put_option_ipv4(writer: &mut Writer, address: Option<Ipv4Addr>) {
    writer.bool(address.is_some());
    put_ipv4(writer, address.unwrap_or(Ipv4Addr::UNSPECIFIED));
}

pub(super) fn get_option_ipv4(reader: &mut Reader<'_>) -> Result<Option<Ipv4Addr>, Status> {
    let present = reader.bool()?;
    let address = get_ipv4(reader)?;
    if !present && !address.is_unspecified() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(present.then_some(address))
}

pub(super) fn put_packet_address(writer: &mut Writer, address: PacketAddress) {
    writer
        .u16(address.protocol)
        .i32(address.interface_index)
        .u16(address.hardware_type)
        .u8(address.packet_type)
        .u8(address.address_length)
        .bytes(&address.address);
}

pub(super) fn get_packet_address(reader: &mut Reader<'_>) -> Result<PacketAddress, Status> {
    let protocol = reader.u16()?;
    let interface_index = reader.i32()?;
    let hardware_type = reader.u16()?;
    let packet_type = reader.u8()?;
    let address_length = reader.u8()?;
    let address = reader
        .bytes()?
        .try_into()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    Ok(PacketAddress {
        protocol,
        interface_index,
        hardware_type,
        packet_type,
        address_length,
        address,
    })
}

pub(super) fn put_inet(writer: &mut Writer, address: InetAddress) {
    match address {
        InetAddress::V4(address) => {
            writer.u8(4);
            put_ipv4(writer, *address.ip());
            writer.u16(address.port());
        }
        InetAddress::V6(address) => {
            writer
                .u8(6)
                .bytes(&address.ip().octets())
                .u16(address.port())
                .u32(address.flowinfo())
                .u32(address.scope_id());
        }
    }
}

pub(super) fn get_inet(reader: &mut Reader<'_>) -> Result<InetAddress, Status> {
    match reader.u8()? {
        4 => Ok(InetAddress::V4(core::net::SocketAddrV4::new(
            get_ipv4(reader)?,
            reader.u16()?,
        ))),
        6 => {
            let address = core::net::Ipv6Addr::from(fixed::<16>(reader)?);
            Ok(InetAddress::V6(core::net::SocketAddrV6::new(
                address,
                reader.u16()?,
                reader.u32()?,
                reader.u32()?,
            )))
        }
        _ => Err(native_abi::ERR_INVALID_ARGS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn control_endpoint_preserves_both_families_port_zone_and_flowinfo_with_exact_layout() {
        let values = [
            InetAddress::V4(core::net::SocketAddrV4::new(
                core::net::Ipv4Addr::new(192, 0, 2, 1),
                0x1234,
            )),
            InetAddress::V6(core::net::SocketAddrV6::new(
                "fe80::abcd".parse().unwrap(),
                0xface,
                0x1234_5678,
                0xdead_beef,
            )),
        ];
        for value in values {
            let mut writer = Writer::request(0);
            put_inet(&mut writer, value);
            let payload = &writer.as_bytes()[8..];
            let mut reader = Reader::new(payload);
            assert_eq!(get_inet(&mut reader), Ok(value));
            assert!(reader.finish().is_ok());
            match value {
                InetAddress::V4(_) => assert_eq!(payload, &[4, 1, 2, 0, 192, 0x34, 0x12]),
                InetAddress::V6(address) => {
                    assert_eq!(&payload[..5], &[6, 16, 0, 0, 0]);
                    assert_eq!(&payload[5..21], &address.ip().octets());
                    assert_eq!(
                        &payload[21..],
                        &[0xce, 0xfa, 0x78, 0x56, 0x34, 0x12, 0xef, 0xbe, 0xad, 0xde]
                    );
                }
            }
            // 不完整 IPv6 字段不能退化为一个有效 IPv4 地址或默认 zone。
            for end in 0..payload.len() {
                assert!(get_inet(&mut Reader::new(&payload[..end])).is_err());
            }
        }
        assert!(get_inet(&mut Reader::new(&[0])).is_err());
        assert!(get_inet(&mut Reader::new(&[255])).is_err());
    }
}
