//! VirtIO 1.4 TX checksum/TSO header 与 RX checksum 补全；不拥有队列或协议状态。
use device_model::network::{
    NetworkChecksum, NetworkError, NetworkTcpSegmentation, NetworkTcpVersion,
};

/// `buffer` 仅包含设备报告的已初始化字节；调用者持有尚未交给客户端的 RX claim。
/// 未协商 GSO，任何非 NONE 的 gso_type 都是设备违反协商；未知 flags 按规范忽略。
pub(super) fn receive(buffer: &mut [u8], negotiated: bool) -> Result<bool, NetworkError> {
    if buffer.len() < 12 || buffer[1] != 0 {
        return Err(NetworkError::Device);
    }
    let flags = buffer[0] & 3;
    if flags != 0 && !negotiated {
        return Err(NetworkError::Device);
    }
    if flags & 1 == 0 {
        // NEEDS_CSUM 未置位时禁止读取 start/offset；hdr_len 不是可信边界。
        return Ok(flags & 2 != 0);
    }
    let start = usize::from(u16::from_le_bytes([buffer[6], buffer[7]]));
    let offset = usize::from(u16::from_le_bytes([buffer[8], buffer[9]]));
    let frame = &mut buffer[12..];
    if start + offset + 2 > frame.len() {
        return Err(NetworkError::Device);
    }
    // seed 已在结果字段；软件补全等价于标准 checksum_help，不把未完成字节交给 RAW/socket。
    let checksum = packet_wire::checksum::checksum(&frame[start..]);
    frame[start + offset..start + offset + 2]
        .copy_from_slice(&if checksum == 0 { 0xffff } else { checksum }.to_be_bytes());
    Ok(true)
}

/// 调用者已通过能力/范围校验且独占 TX 槽；只编码 header，不改 payload 或分配。
pub(super) fn transmit(
    header: &mut [u8],
    checksum: Option<NetworkChecksum>,
    segmentation: Option<NetworkTcpSegmentation>,
) {
    header.fill(0);
    if let Some(checksum) = checksum {
        header[0] = 1; // NEEDS_CSUM；设备不能将其与 DATA_VALID 混淆。
        header[2..4].copy_from_slice(&checksum.header_length.to_le_bytes());
        header[6..8].copy_from_slice(&checksum.start.to_le_bytes());
        header[8..10].copy_from_slice(&checksum.offset.to_le_bytes());
    }
    if let Some(segmentation) = segmentation {
        header[1] = match segmentation.version {
            NetworkTcpVersion::Ipv4 => 1,
            NetworkTcpVersion::Ipv6 => 4,
        } | if segmentation.ecn { 0x80 } else { 0 };
        header[4..6].copy_from_slice(&segmentation.maximum_segment_size.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offload_header_and_bounds_match_negotiated_contract() {
        let mut buffer = [0u8; 20];
        buffer[0] = 1;
        buffer[8] = 2;
        buffer[12..].copy_from_slice(&[0, 1, 0, 0, 0xf4, 0xf5, 0xf6, 0xf7]);
        assert_eq!(receive(&mut buffer, true), Ok(true));
        assert_eq!(&buffer[14..16], &0x1411u16.to_be_bytes());
        assert_eq!(packet_wire::checksum::checksum(&buffer[12..]), 0);
        buffer[8] = 7;
        assert_eq!(receive(&mut buffer, true), Err(NetworkError::Device));
        // DATA_VALID 必须忽略无意义的 checksum offsets；未知 flag 也不能导致丢弃。
        buffer[0] = 0x82;
        buffer[6..10].fill(0xff);
        assert_eq!(receive(&mut buffer, true), Ok(true));
        assert_eq!(receive(&mut buffer, false), Err(NetworkError::Device));
        buffer[0] = 0x80;
        assert_eq!(receive(&mut buffer, false), Ok(false));
        buffer[1] = 1;
        assert_eq!(receive(&mut buffer, true), Err(NetworkError::Device));

        let cap = device_model::network::NetworkTransmitCapabilities {
            checksum: true,
            tcp_ipv4: true,
            tcp_ipv6: true,
            tcp_ecn: true,
            maximum_frame_length: 1514,
            maximum_buffer_length: 65549,
        };
        cap.validate().unwrap();
        for (version, start, kind) in [
            (NetworkTcpVersion::Ipv4, 34, 1),
            (NetworkTcpVersion::Ipv6, 54, 4),
        ] {
            let checksum = NetworkChecksum {
                start,
                offset: 16,
                header_length: start + 20,
            };
            let segmentation = NetworkTcpSegmentation {
                version,
                maximum_segment_size: 1440,
                ecn: true,
            };
            cap.validate_packet(4096, Some(checksum), Some(segmentation))
                .unwrap();
            let mut header = [0xa5; 12];
            transmit(&mut header, Some(checksum), Some(segmentation));
            assert_eq!(header[0], 1);
            assert_eq!(header[1], kind | 0x80);
            assert_eq!(&header[2..4], &(start + 20).to_le_bytes());
            assert_eq!(&header[4..6], &1440u16.to_le_bytes());
            assert_eq!(&header[6..8], &start.to_le_bytes());
            assert_eq!(&header[8..10], &16u16.to_le_bytes());
            assert_eq!(&header[10..12], &[0, 0]);
            assert!(cap.validate_packet(4096, None, Some(segmentation)).is_err());
            assert!(cap.validate_packet(4096, Some(checksum), None).is_err());
            assert!(
                cap.validate_packet(
                    4096,
                    Some(checksum),
                    Some(NetworkTcpSegmentation {
                        maximum_segment_size: 0,
                        ..segmentation
                    })
                )
                .is_err()
            );
            assert!(
                cap.validate_packet(
                    4096,
                    Some(checksum),
                    Some(NetworkTcpSegmentation {
                        maximum_segment_size: 1514,
                        ..segmentation
                    })
                )
                .is_err()
            );
            assert!(
                device_model::network::NetworkTransmitCapabilities {
                    tcp_ecn: false,
                    ..cap
                }
                .validate_packet(4096, Some(checksum), Some(segmentation))
                .is_err()
            );
        }
        let request = device_model::network::NetworkQueueConfiguration {
            queue_pairs: 2,
            transmit_bytes: 65549,
        };
        request.validate(2, cap).unwrap();
        assert!(request.validate(1, cap).is_err());
        assert!(
            device_model::network::NetworkQueueConfiguration {
                transmit_bytes: 65550,
                ..request
            }
            .validate(2, cap)
            .is_err()
        );
    }
}
