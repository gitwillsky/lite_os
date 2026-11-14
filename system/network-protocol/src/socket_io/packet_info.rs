//! 有序发送控制项与接收投影的唯一 wire codec；不包含 Linux cmsg 编号或路由策略。
use super::*;

pub(super) const CONTROL_BYTES: usize = 24;
pub(in super::super) fn payload_length(data: usize, count: usize) -> Result<usize, Status> {
    count
        .checked_mul(CONTROL_BYTES)
        .and_then(|prefix| prefix.checked_add(data))
        .filter(|total| *total <= TRANSFER_LIMIT)
        .ok_or(native_abi::ERR_NO_MEMORY)
}
pub(super) fn encode(control: InternetSendPacketInfo) -> [u8; CONTROL_BYTES] {
    let mut bytes = [0; CONTROL_BYTES];
    let index = match control {
        InternetSendPacketInfo::Ipv4 {
            interface_index,
            source_address,
        } => {
            bytes[0] = 4;
            bytes[8..12].copy_from_slice(&source_address.octets());
            interface_index
        }
        InternetSendPacketInfo::Ipv6 {
            interface_index,
            source_address,
        } => {
            bytes[0] = 6;
            bytes[8..].copy_from_slice(&source_address.octets());
            interface_index
        }
        InternetSendPacketInfo::Invalid(family) => {
            bytes[0] = match family {
                InetFamily::Ipv4 => 0x84,
                InetFamily::Ipv6 => 0x86,
            };
            None
        }
    };
    bytes[4..8].copy_from_slice(&index.unwrap_or(0).to_le_bytes());
    bytes
}
pub(super) fn decode(bytes: &[u8]) -> Result<InternetSendPacketInfo, Status> {
    let bytes: &[u8; CONTROL_BYTES] = bytes.try_into().map_err(|_| invalid())?;
    if bytes[1..4] != [0; 3] {
        return Err(invalid());
    }
    let index = u32::from_le_bytes(bytes[4..8].try_into().expect("fixed index"));
    let interface_index = (index != 0).then_some(index);
    match bytes[0] {
        4 if bytes[12..] == [0; 12] => Ok(InternetSendPacketInfo::Ipv4 {
            interface_index,
            source_address: Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[8..12]).expect("fixed IPv4")),
        }),
        6 => Ok(InternetSendPacketInfo::Ipv6 {
            interface_index,
            source_address: core::net::Ipv6Addr::from(
                <[u8; 16]>::try_from(&bytes[8..]).expect("fixed IPv6"),
            ),
        }),
        0x84 | 0x86 if bytes[4..] == [0; 20] => {
            Ok(InternetSendPacketInfo::Invalid(if bytes[0] == 0x84 {
                InetFamily::Ipv4
            } else {
                InetFamily::Ipv6
            }))
        }
        _ => Err(invalid()),
    }
}
pub(super) fn receive_info(record: &mut Record, info: Option<InternetPacketInfo>) {
    record[8..12].copy_from_slice(&encode_receive_info(info));
}

pub(super) fn encode_receive_info(info: Option<InternetPacketInfo>) -> [u64; 4] {
    let mut fields = [0; 4];
    match info {
        Some(InternetPacketInfo::Ipv4 {
            interface_index,
            specific_address,
            destination,
        })
        | Some(InternetPacketInfo::DualStack {
            interface_index,
            specific_address,
            destination,
        }) => {
            fields[0] = if matches!(info, Some(InternetPacketInfo::DualStack { .. })) {
                3
            } else {
                1
            };
            fields[1] = u64::from(interface_index);
            fields[2] = u64::from(u32::from_be_bytes(specific_address.octets()));
            fields[3] = u64::from(u32::from_be_bytes(destination.octets()));
        }
        Some(InternetPacketInfo::Ipv6 {
            interface_index,
            destination,
        }) => {
            fields[0] = 2;
            fields[1] = u64::from(interface_index);
            let bytes = destination.octets();
            fields[2] = u64::from_be_bytes(bytes[..8].try_into().expect("fixed IPv6 half"));
            fields[3] = u64::from_be_bytes(bytes[8..].try_into().expect("fixed IPv6 half"));
        }
        None => {}
    }
    fields
}

pub(in super::super) fn get_receive_info(
    record: &Record,
) -> Result<Option<InternetPacketInfo>, Status> {
    decode_receive_info(record[8..12].try_into().expect("fixed packet-info fields"))
}

pub(super) fn decode_receive_info(fields: &[u64; 4]) -> Result<Option<InternetPacketInfo>, Status> {
    let interface_index = narrow(fields[1])?;
    let info = match fields[0] {
        0 if fields[1..] == [0; 3] => return Ok(None),
        1 => InternetPacketInfo::Ipv4 {
            interface_index,
            specific_address: ipv4(fields[2])?,
            destination: ipv4(fields[3])?,
        },
        3 => InternetPacketInfo::DualStack {
            interface_index,
            specific_address: ipv4(fields[2])?,
            destination: ipv4(fields[3])?,
        },
        2 => {
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&fields[2].to_be_bytes());
            bytes[8..].copy_from_slice(&fields[3].to_be_bytes());
            InternetPacketInfo::Ipv6 {
                interface_index,
                destination: bytes.into(),
            }
        }
        _ => return Err(invalid()),
    };
    if interface_index == 0 {
        return Err(invalid());
    }
    Ok(Some(info))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 现有地址记录用例无法捕获控制前缀越界、类型混淆或双栈接收信息丢失。
    #[test]
    fn controls_have_fixed_width_canonical_tags_and_checked_combined_capacity() {
        let v4 = InternetSendPacketInfo::Ipv4 {
            interface_index: Some(0xdead_beef),
            source_address: Ipv4Addr::new(192, 0, 2, 1),
        };
        let mut expected = [0; CONTROL_BYTES];
        expected[0] = 4;
        expected[4..8].copy_from_slice(&[0xef, 0xbe, 0xad, 0xde]);
        expected[8..12].copy_from_slice(&[192, 0, 2, 1]);
        assert_eq!(encode(v4), expected);
        for item in [
            v4,
            InternetSendPacketInfo::Ipv6 {
                interface_index: None,
                source_address: "fe80::1".parse().unwrap(),
            },
            InternetSendPacketInfo::Invalid(InetFamily::Ipv4),
            InternetSendPacketInfo::Invalid(InetFamily::Ipv6),
        ] {
            assert_eq!(decode(&encode(item)), Ok(item));
        }
        for (index, value) in [(0, 7), (1, 1), (12, 1)] {
            let mut bad = expected;
            bad[index] = value;
            assert!(decode(&bad).is_err());
        }
        let mut invalid = encode(InternetSendPacketInfo::Invalid(InetFamily::Ipv6));
        invalid[4] = 1;
        assert!(decode(&invalid).is_err());
        assert!(decode(&expected[..23]).is_err());
        assert_eq!(
            payload_length(TRANSFER_LIMIT - CONTROL_BYTES, 1),
            Ok(TRANSFER_LIMIT)
        );
        assert!(payload_length(TRANSFER_LIMIT, 1).is_err());
        assert!(payload_length(0, usize::MAX).is_err());
    }

    #[test]
    fn receive_info_preserves_both_mapped_projections_and_rejects_reserved_fields() {
        let v4 = Ipv4Addr::new(192, 0, 2, 1);
        for info in [
            None,
            Some(InternetPacketInfo::Ipv4 {
                interface_index: 2,
                specific_address: v4,
                destination: Ipv4Addr::BROADCAST,
            }),
            Some(InternetPacketInfo::DualStack {
                interface_index: 2,
                specific_address: v4,
                destination: Ipv4Addr::BROADCAST,
            }),
            Some(InternetPacketInfo::Ipv6 {
                interface_index: 2,
                destination: "fe80::1234".parse().unwrap(),
            }),
        ] {
            let mut record = [0; queue::WORDS];
            receive_info(&mut record, info);
            assert_eq!(get_receive_info(&record), Ok(info));
            if let Some(InternetPacketInfo::DualStack { .. }) = info {
                assert_eq!(
                    get_receive_info(&record).unwrap().unwrap().ipv4(),
                    Some((2, v4, Ipv4Addr::BROADCAST))
                );
                assert_eq!(
                    get_receive_info(&record).unwrap().unwrap().ipv6(),
                    Some((2, Ipv4Addr::BROADCAST.to_ipv6_mapped()))
                );
            }
            record[9] = if info.is_some() { 0 } else { 1 };
            assert!(get_receive_info(&record).is_err());
        }
        let mut record = [0; queue::WORDS];
        record[8..12].copy_from_slice(&[1, 2, 1 << 32, 0]);
        assert!(get_receive_info(&record).is_err());
        record[8..12].copy_from_slice(&[2, 1 << 32, 0, 1]);
        assert!(get_receive_info(&record).is_err());
    }
}
