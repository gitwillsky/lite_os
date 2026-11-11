//! RFC 6793 的有效路径视图；格式段借用原包，重建不复制 AS 号或改变原属性。
use crate::Aggregator;
use packet_wire::{
    PacketError,
    bgp::attributes::{AsPath, AsnWidth, Attribute, Segment, SegmentKind},
};
mod resident;
pub use resident::{PathPool, StoredPath};

/// 完成 AS4 重建的路径；去环和选路应使用此视图，不直接解释收到的 AS_PATH。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Path<'a> {
    base: AsPath<'a>,
    auxiliary: Option<AsPath<'a>>,
    leading: usize,
    // 原辅助属性的 Partial 必须沿 OLD 方向保留；只影响传播元数据，不改变 ASN 内容身份。
    auxiliary_partial: bool,
}

/// 有效路径中的一段；序列可在重建边界截短，集合保持完整。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathSegment<'a> {
    wire: Segment<'a>,
    count: usize,
}
impl PathSegment<'_> {
    /// 保留集合、序列及联盟段类型，不能把 AS_SET 当成有序序列。
    pub fn kind(self) -> SegmentKind {
        self.wire.kind
    }
    /// 按原顺序返回本段的有效 AS 号；所有值统一为四字节整数。
    pub fn asns(self) -> impl ExactSizeIterator<Item = u32> + Clone {
        self.wire.asns().take(self.count)
    }
}
fn hops(segment: Segment<'_>) -> usize {
    match segment.kind {
        SegmentKind::Sequence => segment.asns().len(),
        SegmentKind::Set => 1,
        SegmentKind::ConfederationSequence | SegmentKind::ConfederationSet => 0,
    }
}
impl<'a> Path<'a> {
    pub(super) fn reconstruct(base: AsPath<'a>, auxiliary: Option<(AsPath<'a>, bool)>) -> Self {
        // 1. 选路长度：序列逐 AS 计数，集合为一跳，联盟段不占跳数。
        let length = base.segments().map(hops).sum::<usize>();
        let auxiliary = auxiliary.and_then(|(path, partial)| {
            let count = path.segments().map(hops).sum::<usize>();
            // 全部联盟段被丢弃后没有可恢复信息；保留基础路径，包括其零长度联盟段。
            (count != 0 && count <= length).then_some((path, count, partial))
        });
        // 2. 只保留基础路径前缀的差额；过长辅助路径不得覆盖基础路径。
        let leading = length - auxiliary.map_or(0, |(_, count, _)| count);
        Self {
            base,
            auxiliary: auxiliary.map(|(path, _, _)| path),
            leading,
            auxiliary_partial: auxiliary.is_some_and(|(_, _, partial)| partial),
        }
    }
    /// 返回有序有效段；基础前缀可截短序列，AS4 中的联盟段被丢弃。
    pub fn segments(self) -> impl Iterator<Item = PathSegment<'a>> + Clone {
        let mut base = self.base.segments();
        let mut remaining = self.leading;
        let mut finished = false;
        let leading = core::iter::from_fn(move || {
            if finished {
                return None;
            }
            let segment = base.next()?;
            let cost = hops(segment);
            if cost != 0 && remaining == 0 {
                finished = true;
                return None;
            }
            let count = if cost > remaining {
                // 3. 只有序列能跨越边界；截短后不再取其后面的联盟段。
                finished = true;
                remaining
            } else {
                segment.asns().len()
            };
            remaining = remaining.saturating_sub(cost);
            Some(PathSegment {
                wire: segment,
                count,
            })
        });
        let auxiliary = self
            .auxiliary
            .into_iter()
            .flat_map(|path| path.segments())
            .filter(|segment| matches!(segment.kind, SegmentKind::Sequence | SegmentKind::Set))
            .map(|segment| PathSegment {
                wire: segment,
                count: segment.asns().len(),
            });
        leading.chain(auxiliary)
    }
    /// RFC 4271/5065 的选路跳数，始终与基础路径相同；不是 AS 号条数。
    pub fn hop_count(self) -> usize {
        self.base.segments().map(hops).sum()
    }
    /// 在重建后的所有段中检查 AS 号，包含集合和保留的联盟段。
    pub fn contains(self, asn: u32) -> bool {
        self.segments()
            .any(|segment| segment.asns().any(|value| value == asn))
    }
    /// 被实际用于重建的 AS4_PATH 是否携 Partial；被忽略的辅助属性不影响此值。
    pub fn auxiliary_partial(self) -> bool {
        self.auxiliary_partial
    }
    /// 按发送方向已协商的 width 写路径与可选聚合器的完整属性序列，返回长度。
    /// OLD 方向用 AS_TRANS 映射不可表示的 AS，按需附 AS4_PATH/AS4_AGGREGATOR；NEW 不发辅助属性。
    /// 本方法不执行出口政策、本地 AS prepend 或联盟边界清理；调用前必须形成最终出口路径。
    /// # Errors
    /// AS 号为零、OLD 方向含不可表示的联盟 AS、属性/列表超过 u16 或容量不足；失败不修改输出。
    pub fn emit_attributes(
        self,
        bytes: &mut [u8],
        width: AsnWidth,
        aggregator: Option<Aggregator>,
    ) -> Result<usize, PacketError> {
        emit_path_attributes(
            self.segments()
                .map(|segment| (segment.kind(), segment.asns())),
            Some(bytes),
            width,
            aggregator,
            self.auxiliary_partial,
        )
    }
}

fn emit_path_attributes<S, A>(
    segments: S,
    bytes: Option<&mut [u8]>,
    width: AsnWidth,
    aggregator: Option<Aggregator>,
    auxiliary_partial: bool,
) -> Result<usize, PacketError>
where
    S: Clone + Iterator<Item = (SegmentKind, A)>,
    A: ExactSizeIterator<Item = u32>,
{
    let old = width == AsnWidth::Two;
    let mappable = |asn| asn <= u32::from(u16::MAX);
    // 1. OLD 联盟内不能借 AS4_PATH 恢复不可表示的成员号；拒绝静默截断或泄露联盟段。
    for (kind, mut asns) in segments.clone() {
        let confederation = matches!(
            kind,
            SegmentKind::ConfederationSequence | SegmentKind::ConfederationSet
        );
        if asns.any(|asn| asn == 0 || old && confederation && !mappable(asn)) {
            return Err(PacketError::Malformed);
        }
    }
    if aggregator.is_some_and(|aggregator| aggregator.asn == 0) {
        return Err(PacketError::Malformed);
    }
    let map_asn = |asn| if old && !mappable(asn) { 23456 } else { asn };
    let base = segments
        .clone()
        .map(|(kind, asns)| (kind, asns.map(map_asn)));
    let auxiliary =
        segments.filter(|(kind, _)| matches!(kind, SegmentKind::Sequence | SegmentKind::Set));
    let as4 = old
        && auxiliary
            .clone()
            .any(|(_, mut asns)| asns.any(|asn| !mappable(asn)));
    let base_length = AsPath::encoded_len(width, base.clone())?;
    let auxiliary_length = if as4 {
        Some(AsPath::encoded_len(AsnWidth::Four, auxiliary.clone())?)
    } else {
        None
    };
    // 2. 预检整批尺寸；后续值编码器只写最终区间，不创建 AS 数组或中间属性副本。
    let mut length = Attribute::encoded_len(base_length)?;
    if let Some(size) = auxiliary_length {
        length += Attribute::encoded_len(size)?;
    }
    if let Some(Aggregator { asn, .. }) = aggregator {
        length += Attribute::aggregator_len(width);
        if old && !mappable(asn) {
            length += Attribute::aggregator_len(AsnWidth::Four);
        }
    }
    u16::try_from(length).map_err(|_| PacketError::Malformed)?;
    // 同一条预检路径既支持精确尺寸也支持最终输出，避免每帧清零最大缓冲或另写尺寸算法。
    let Some(bytes) = bytes else {
        return Ok(length);
    };
    let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
    let mut at = Attribute::emit_with(bytes, 0x40, 2, base_length, |output| {
        AsPath::emit(output, width, base).expect("path dimensions prevalidated");
    })
    .expect("attribute dimensions prevalidated");
    if let Some(size) = auxiliary_length {
        at += Attribute::emit_with(
            &mut bytes[at..],
            if auxiliary_partial { 0xe0 } else { 0xc0 },
            17,
            size,
            |output| {
                AsPath::emit(output, AsnWidth::Four, auxiliary)
                    .expect("AS4 dimensions prevalidated");
            },
        )
        .expect("AS4 attribute dimensions prevalidated");
    }
    // 3. 真实聚合器与路径使用相同的方向宽度；只在不可映射时额外携带 AS4 聚合器。
    if let Some(Aggregator {
        asn,
        identifier,
        partial,
    }) = aggregator
    {
        at += Attribute::emit_aggregator(
            &mut bytes[at..],
            7,
            width,
            map_asn(asn),
            identifier,
            partial,
        )
        .expect("aggregator dimensions prevalidated");
        if old && !mappable(asn) {
            Attribute::emit_aggregator(
                &mut bytes[at..],
                18,
                AsnWidth::Four,
                asn,
                identifier,
                partial,
            )
            .expect("AS4 aggregator dimensions prevalidated");
        }
    }
    Ok(length)
}
