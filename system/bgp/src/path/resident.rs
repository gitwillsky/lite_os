//! 路径规范化与驻留；索引只弱引用共享句柄，最后一个路由引用释放时立即移除索引。
use super::{Path, PathSegment};
use crate::Aggregator;
use crate::residency::{Pool, Shared};
use alloc::vec::Vec;
use fallible_tree::OutOfMemory;
use packet_wire::{
    PacketError,
    bgp::attributes::{AsnWidth, SegmentKind},
};

fn set_members<'a>(segment: PathSegment<'_>, scratch: &'a mut [u32; 255]) -> &'a [u32] {
    // wire 段最多 255 项；借用视图只可能截短，故此窗口覆盖全部集合，不按恶意重复数长期预留容量。
    let count = segment.asns().len();
    for (output, asn) in scratch[..count].iter_mut().zip(segment.asns()) {
        *output = asn;
    }
    scratch[..count].sort_unstable();
    let mut end = 0;
    for read in 0..count {
        if end == 0 || scratch[read] != scratch[end - 1] {
            scratch[end] = scratch[read];
            end += 1;
        }
    }
    &scratch[..end]
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Span {
    kind: SegmentKind,
    end: u32,
}
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalPath {
    spans: Vec<Span>,
    asns: Vec<u32>,
}
impl CanonicalPath {
    fn capture(path: Path<'_>) -> Result<Self, OutOfMemory> {
        let mut spans = Vec::<Span>::new();
        let mut asns = Vec::new();
        let mut scratch = [0; 255];
        // 1. 计算规范化后的真实容量，而非保留输入的重复项/碎片化段上限。
        let mut count = 0;
        let mut span_count = 0;
        let mut previous = None;
        for segment in path.segments() {
            let kind = segment.kind();
            count += if matches!(kind, SegmentKind::Set | SegmentKind::ConfederationSet) {
                set_members(segment, &mut scratch).len()
            } else {
                segment.asns().len()
            };
            if previous != Some(kind)
                || !matches!(
                    kind,
                    SegmentKind::Sequence | SegmentKind::ConfederationSequence
                )
            {
                span_count += 1;
            }
            previous = Some(kind);
        }
        // 2. 分配失败不向驻留表发表任何状态。
        spans
            .try_reserve_exact(span_count)
            .map_err(|_| OutOfMemory)?;
        asns.try_reserve_exact(count).map_err(|_| OutOfMemory)?;
        // 3. 填入预留容量，保留序列顺序/次数及集合边界；此阶段不再分配。
        for segment in path.segments() {
            let kind = segment.kind();
            if matches!(kind, SegmentKind::Set | SegmentKind::ConfederationSet) {
                // 集合只有成员关系；只排序/去重本段，绝不能删掉序列中的重复 prepend。
                asns.extend_from_slice(set_members(segment, &mut scratch));
            } else {
                asns.extend(segment.asns());
            }
            if let Some(previous) = spans.last_mut().filter(|span| {
                span.kind == kind
                    && matches!(
                        kind,
                        SegmentKind::Sequence | SegmentKind::ConfederationSequence
                    )
            }) {
                previous.end = u32::try_from(asns.len()).expect("wire-backed path bounds AS count");
            } else {
                spans.push(Span {
                    kind,
                    end: u32::try_from(asns.len()).expect("wire-backed path bounds AS count"),
                });
            }
        }
        Ok(Self { spans, asns })
    }
    fn segments(&self) -> impl Iterator<Item = (SegmentKind, &[u32])> + Clone {
        let mut start = 0;
        self.spans.iter().map(move |span| {
            let end = span.end as usize;
            let values = &self.asns[start..end];
            start = end;
            (span.kind, values)
        })
    }
}

/// 单个 BGP actor 的路径驻留表；不持全局状态或锁，不允许跨线程发送。
pub struct PathPool(Pool<CanonicalPath>);
impl PathPool {
    /// 创建空驻留表。
    /// # Errors
    /// 共享控制块分配失败。
    pub fn try_new() -> Result<Self, OutOfMemory> {
        Pool::try_new().map(Self)
    }
    /// 返回仍被引用的不同规范路径数量。
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// 是否没有任何驻留路径。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 规范化并复用已有共享内容；不会把 wire 表示当作 canonical key。
    /// # Errors
    /// 捕获或驻留分配失败；原索引/已有句柄不变。
    pub fn intern(&mut self, path: Path<'_>) -> Result<StoredPath, OutOfMemory> {
        self.0.intern(CanonicalPath::capture(path)?).map(StoredPath)
    }
}

/// 可廉价克隆的不可变路径引用，每个路由只需一个指针；不保活驻留表，表释放后仍可读取。
/// Eq/Ord 比较规范内容，仅用于身份/索引；Ord 不是 BGP 路径优先级。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoredPath(Shared<CanonicalPath>);
impl StoredPath {
    /// 返回逻辑规范段；相邻同类序列已合并，集合已排序去重，有序 prepend 保持次数。
    /// 逻辑序列可以超过线上 255 项限制，生成报文时必须按段上限重新分段。
    pub fn segments(&self) -> impl Iterator<Item = (SegmentKind, &[u32])> + Clone {
        self.0.get().segments()
    }
    /// RFC 4271/5065 选路跳数：集合一跳、联盟零跳；不因规范化改变长度。
    pub fn hop_count(&self) -> usize {
        self.segments()
            .map(|(kind, asns)| match kind {
                SegmentKind::Sequence => asns.len(),
                SegmentKind::Set => 1,
                SegmentKind::ConfederationSequence | SegmentKind::ConfederationSet => 0,
            })
            .sum()
    }
    /// 检查规范路径中的 AS 号，包含集合与联盟段，供去环准入使用。
    pub fn contains(&self, asn: u32) -> bool {
        self.0.get().asns.contains(&asn)
    }
    /// 将驻留逻辑路径重新分段并沿唯一 AS4 发送投影写最终属性；传播元数据由完整属性 owner 提供。
    /// # Errors
    /// 与 Path::emit_attributes 相同：非法 AS/联盟迁移、属性长度越界或容量不足时不写输出。
    pub(crate) fn emit_attributes(
        &self,
        bytes: &mut [u8],
        width: AsnWidth,
        aggregator: Option<Aggregator>,
        auxiliary_partial: bool,
    ) -> Result<usize, PacketError> {
        let segments = self.segments().flat_map(|(kind, asns)| {
            asns.chunks(255)
                .map(move |chunk| (kind, chunk.iter().copied()))
        });
        super::emit_path_attributes(segments, bytes, width, aggregator, auxiliary_partial)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use SegmentKind::{ConfederationSequence as Confed, Sequence as Seq, Set};
    use alloc::vec;
    use packet_wire::bgp::attributes::{AsPath, Attributes, Value};

    fn capture(pool: &mut PathPool, segments: &[(SegmentKind, &[u32])]) -> StoredPath {
        let mut bytes = vec![0; 8192];
        let size = AsPath::emit(
            &mut bytes,
            AsnWidth::Four,
            segments
                .iter()
                .map(|(kind, asns)| (*kind, asns.iter().copied())),
        )
        .unwrap();
        let wire = AsPath::parse(&bytes[..size], AsnWidth::Four).unwrap();
        pool.intern(Path::reconstruct(wire, None)).unwrap()
    }

    #[test]
    fn equivalent_path_representations_share_one_resident_without_erasing_prepend_or_set_boundaries()
     {
        // 借用路径的 Eq 不能保护这个契约；段边界、集合顺序或重复不同会导致全表驻留重复膨胀。
        let mut pool = PathPool::try_new().unwrap();
        let a = capture(
            &mut pool,
            &[
                (Seq, &[65001]),
                (Seq, &[65002, 65002]),
                (Set, &[80000, 70000, 80000]),
            ],
        );
        let b = capture(
            &mut pool,
            &[(Seq, &[65001, 65002, 65002]), (Set, &[70000, 80000])],
        );
        assert!(core::ptr::eq(a.0.get(), b.0.get()));
        assert_eq!(pool.len(), 1);
        assert_eq!(a.hop_count(), 4);
        assert!(a.contains(70000));
        assert!(!a.contains(65003));
        let shorter = capture(&mut pool, &[(Seq, &[65001, 65002]), (Set, &[70000, 80000])]);
        assert_ne!(a, shorter);
        let split_sets = capture(
            &mut pool,
            &[
                (Seq, &[65001, 65002, 65002]),
                (Set, &[70000]),
                (Set, &[80000]),
            ],
        );
        assert_ne!(a, split_sets);
        assert_eq!(split_sets.hop_count(), 5);
        assert_eq!(pool.len(), 3);
        let confed = capture(
            &mut pool,
            &[(Confed, &[65010]), (Confed, &[65011]), (Seq, &[65001])],
        );
        let same = capture(&mut pool, &[(Confed, &[65010, 65011]), (Seq, &[65001])]);
        assert!(core::ptr::eq(confed.0.get(), same.0.get()));
        assert_eq!(confed.hop_count(), 1);
    }

    #[test]
    fn last_route_reference_removes_only_its_index_and_survives_pool_teardown() {
        // 验证真实 Rc/Weak/索引生命周期，防止弱索引留死 key、互相保活，或池退出使路由引用悬空。
        let mut pool = PathPool::try_new().unwrap();
        let a = capture(&mut pool, &[(Seq, &[65001])]);
        let b = capture(&mut pool, &[(Seq, &[65002])]);
        let clone = a.clone();
        drop(a);
        assert_eq!(pool.len(), 2);
        drop(clone);
        assert_eq!(pool.len(), 1);
        assert!(b.contains(65002));
        drop(b);
        assert!(pool.is_empty());
        let empty = capture(&mut pool, &[]);
        let same = capture(&mut pool, &[]);
        assert!(core::ptr::eq(empty.0.get(), same.0.get()));
        drop(pool);
        assert_eq!(empty.hop_count(), 0);
        assert_eq!(same.segments().count(), 0);
    }

    #[test]
    fn merged_sequence_is_resegmented_on_wire_and_restores_four_octet_asns_for_old_peer() {
        // 合并后的逻辑序列超过 255 项；不能只靠原始段 codec 用例证明它仍能合法传播。
        let mut pool = PathPool::try_new().unwrap();
        let path = capture(
            &mut pool,
            &[
                (Seq, &[65001; 255]),
                (Seq, &[65001; 255]),
                (Seq, &[70000; 90]),
            ],
        );
        assert_eq!(path.segments().count(), 1);
        assert_eq!(path.hop_count(), 600);
        for width in [AsnWidth::Two, AsnWidth::Four] {
            let mut bytes = vec![0; 8192];
            let size = path
                .emit_attributes(&mut bytes, width, None, false)
                .unwrap();
            let mut attributes = Attributes::parse(&bytes[..size]).unwrap();
            let Value::AsPath(base) = attributes.next().unwrap().decode(width).unwrap() else {
                panic!("AS_PATH")
            };
            assert!(
                base.segments()
                    .map(|segment| segment.asns().len())
                    .eq([255, 255, 90])
            );
            let auxiliary = if width == AsnWidth::Two {
                let Value::AsPath(auxiliary) = attributes.next().unwrap().decode(width).unwrap()
                else {
                    panic!("AS4_PATH")
                };
                Some(auxiliary)
            } else {
                None
            };
            assert!(attributes.next().is_none());
            let restored = pool
                .intern(Path::reconstruct(base, auxiliary.map(|path| (path, false))))
                .unwrap();
            assert!(core::ptr::eq(path.0.get(), restored.0.get()));
        }
    }
}
