//! 出口属性直接写最终 UPDATE；AS4、属性头与 NLRI 均复用唯一格式 owner。
use super::{Advertisement, ExportContext, ExportError};
use alloc::vec::Vec;
use packet_wire::{
    PacketError,
    bgp::{
        self,
        attributes::{Attribute, MpReach, MpUnreach},
        nlri::{Family, Nlri, Prefix},
        update::Update,
    },
};

// WRITE=false 与最终写入共用字段选择；编译期消去写入分支，只分配并清零实际报文长度。
struct Attributes<'a, const WRITE: bool> {
    output: &'a mut [u8],
    at: usize,
}
impl<const WRITE: bool> Attributes<'_, WRITE> {
    fn emit(&mut self, flags: u8, code: u8, data: &[u8]) -> Result<(), PacketError> {
        self.at += if WRITE {
            Attribute::emit_with(
                &mut self.output[self.at..],
                flags,
                code,
                data.len(),
                |output| output.copy_from_slice(data),
            )?
        } else {
            Attribute::encoded_len(data.len())?
        };
        Ok(())
    }
    fn sequence<T>(
        &mut self,
        flags: u8,
        code: u8,
        values: impl Iterator<Item = T> + Clone,
        width: usize,
        write: impl Fn(T, &mut [u8]),
    ) -> Result<(), PacketError> {
        let length = values
            .clone()
            .count()
            .checked_mul(width)
            .ok_or(PacketError::Malformed)?;
        if length == 0 {
            return Ok(());
        }
        // 当前方向只允许标准消息，单个集合已超过上限时是逐目标不可发送，不是域格式损坏。
        if length > bgp::STANDARD_LIMIT {
            return Err(PacketError::Capacity);
        }
        self.at += if WRITE {
            Attribute::emit_with(&mut self.output[self.at..], flags, code, length, |output| {
                for (value, bytes) in values.zip(output.chunks_exact_mut(width)) {
                    write(value, bytes);
                }
            })?
        } else {
            Attribute::encoded_len(length)?
        };
        Ok(())
    }
}
/// 同一批共享的实际属性长度；逐目标合批只增算 NLRI/MP 头，不重复遍历属性。
pub(super) fn attributes_length(
    route: Option<&Advertisement>,
    context: ExportContext,
) -> Result<usize, ExportError> {
    Ok(route.map_or(Ok(0), |route| {
        emit_attributes::<false>(&mut [], route, context)
    })?)
}
// MP_REACH 的 AFI/SAFI、长度和 reserved 共 5 bytes，下一跳为 16 或 32；MP_UNREACH 只有 3 bytes。
fn mp_header(route: Option<&Advertisement>) -> usize {
    route.map_or(3, |route| {
        21 + usize::from(route.next_hop.link_local.is_some()) * 16
    })
}
/// 包含 MP 属性头和 NLRI 的精确长度；跨 255 字节时同步计入扩展长度字节。
pub(super) fn message_length(
    attributes: usize,
    route: Option<&Advertisement>,
    multiprotocol: bool,
    nlri_size: usize,
) -> Result<usize, ExportError> {
    let payload = if multiprotocol {
        Attribute::encoded_len(mp_header(route) + nlri_size)?
    } else {
        nlri_size
    };
    Ok(bgp::HEADER_BYTES + 4 + attributes + payload)
}
pub(super) fn message(
    prefixes: impl Iterator<Item = Prefix>,
    route: Option<&Advertisement>,
    context: ExportContext,
    family: Family,
    multiprotocol: bool,
) -> Result<Vec<u8>, ExportError> {
    // 128 个 /128 的最坏 NLRI 2176 bytes，只在当前帧借用栈，不复制属性或分配中间正文。
    let mut nlri = [0; super::EXPORT_BATCH * 17];
    let mut size = 0;
    for prefix in prefixes {
        size += Nlri::emit(&mut nlri[size..], family, &[prefix])?;
    }
    let capacity = message_length(
        attributes_length(route, context)?,
        route,
        multiprotocol,
        size,
    )?;
    if capacity > bgp::STANDARD_LIMIT {
        return Err(PacketError::Capacity.into());
    }
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| ExportError::NoMemory)?;
    encoded.resize(capacity, 0);
    let prefix = Nlri::parse(&nlri[..size], family)?;
    let empty = Nlri::parse(&[], Family::Ipv4)?;
    let length = Update::emit_with(
        &mut encoded,
        if !multiprotocol && route.is_none() {
            prefix
        } else {
            empty
        },
        if !multiprotocol && route.is_some() {
            prefix
        } else {
            empty
        },
        false,
        |output| {
            if !multiprotocol {
                return route.map_or(Ok(0), |route| {
                    emit_attributes::<true>(output, route, context)
                });
            }
            let (code, length) = if route.is_some() {
                (14, mp_header(route) + size)
            } else {
                (15, 3 + size)
            };
            let mut result = Ok(0);
            let length = Attribute::emit_with(output, 0x80, code, length, |body| {
                result = if let Some(route) = route {
                    let core::net::IpAddr::V6(hop) = route.next_hop.address else {
                        unreachable!("family-checked advertisement")
                    };
                    let mut next_hop = [0; 32];
                    next_hop[..16].copy_from_slice(&hop.octets());
                    let length = if let Some(local) = route.next_hop.link_local {
                        next_hop[16..].copy_from_slice(&local.octets());
                        32
                    } else {
                        16
                    };
                    MpReach::emit(body, 1, &next_hop[..length], prefix)
                } else {
                    MpUnreach::emit(body, 1, prefix)
                };
            })?;
            result?;
            let trailing = route.map_or(Ok(0), |route| {
                emit_attributes::<true>(&mut output[length..], route, context)
            })?;
            Ok(length + trailing)
        },
    )?;
    encoded.truncate(length);
    Ok(encoded)
}

fn emit_attributes<const WRITE: bool>(
    output: &mut [u8],
    route: &Advertisement,
    context: ExportContext,
) -> Result<usize, PacketError> {
    let attributes = &route.attributes;
    let external = context.target.kind == crate::PeerKind::External;
    let mut writer = Attributes::<WRITE> { output, at: 0 };
    writer.emit(0x40, 1, &[attributes.origin()])?;
    // 本方向只发送标准消息；先用 ASN 字节数下界排除巨型本地编辑，不能将 u16 容量限制升级为域损坏。
    let asns = attributes
        .path()
        .segments()
        .filter(|(kind, _)| {
            !external
                || matches!(
                    kind,
                    packet_wire::bgp::attributes::SegmentKind::Sequence
                        | packet_wire::bgp::attributes::SegmentKind::Set
                )
        })
        .try_fold(
            usize::from(context.target.kind != crate::PeerKind::Internal),
            |count, (_, values)| count.checked_add(values.len()),
        )
        .ok_or(PacketError::Capacity)?;
    let width = if context.width == packet_wire::bgp::attributes::AsnWidth::Four {
        4
    } else {
        2
    };
    if asns
        .checked_mul(width)
        .is_none_or(|bytes| bytes > bgp::STANDARD_LIMIT)
    {
        return Err(PacketError::Capacity);
    }
    writer.at += attributes.path().emit_export(
        if WRITE {
            Some(&mut writer.output[writer.at..])
        } else {
            None
        },
        context.width,
        context.target.kind,
        context.local_as,
        attributes.aggregator(),
        attributes.is_partial(17),
    )?;
    if let core::net::IpAddr::V4(next_hop) = route.next_hop.address {
        writer.emit(0x40, 3, &next_hop.octets())?;
    }
    if let Some(preference) = route.preference {
        writer.emit(0x40, 5, &preference.to_be_bytes())?;
    }
    // 普通上游 MED 已在出口投影中删除；显式本地 Set 可携带，LOCAL_PREF/反射字段仍不向外部泄露。
    if let Some(med) = attributes.med() {
        writer.emit(0x80, 4, &med.to_be_bytes())?;
    }
    if attributes.atomic_aggregate() {
        writer.emit(0x40, 6, &[])?;
    }
    let flags = |code| {
        if attributes.is_partial(code) {
            0xe0
        } else {
            0xc0
        }
    };
    writer.sequence(
        flags(8),
        8,
        attributes.communities().iter().copied(),
        4,
        |value, bytes| bytes.copy_from_slice(&value.to_be_bytes()),
    )?;
    writer.sequence(
        flags(16),
        16,
        attributes
            .extended_communities()
            .iter()
            .filter(|value| !external || value[0] & 0x40 == 0),
        8,
        |value, bytes| bytes.copy_from_slice(value),
    )?;
    writer.sequence(
        flags(32),
        32,
        attributes.large_communities().iter(),
        12,
        |value, bytes| bytes.copy_from_slice(value),
    )?;
    if context.target.kind == crate::PeerKind::Internal {
        if let Some(identifier) = route
            .reflected_originator
            .or_else(|| attributes.originator_id())
        {
            writer.emit(0x80, 9, &identifier.octets())?;
        }
        writer.sequence(
            0x80,
            10,
            route
                .reflected_originator
                .and(context.cluster_id)
                .into_iter()
                .chain(attributes.cluster_list().iter().copied()),
            4,
            |value, bytes| bytes.copy_from_slice(&value.octets()),
        )?;
    }
    for attribute in attributes.unknown_transitive() {
        writer.emit(attribute.flags, attribute.code, attribute.data)?;
    }
    Ok(writer.at)
}
