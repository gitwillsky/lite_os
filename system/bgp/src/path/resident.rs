//! 路径规范化与驻留；索引只弱引用共享句柄，最后一个路由引用释放时立即移除索引。
use super::{Path, PathSegment};
use crate::Aggregator;
use crate::residency::{Pool, Shared};
use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroU32;
use fallible_tree::OutOfMemory;
use packet_wire::{
    PacketError,
    bgp::attributes::{AsnWidth, SegmentKind},
};
use router_protocol::bgp::AsPathReplacement;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

// 统一路径编辑视图：只借用配置/原路径，不先捕获“删除后”中间路径或按前缀缓存。
struct Projection<'a> {
    source: &'a CanonicalPath,
    excluded: &'a [NonZeroU32],
    replacement: Option<&'a AsPathReplacement>,
    prepended: &'a [NonZeroU32],
}
impl Projection<'_> {
    fn retained(&self, value: u32) -> bool {
        self.excluded
            .binary_search_by_key(&value, |asn| asn.get())
            .is_err()
    }
    fn mapped<'a>(
        &'a self,
        kind: SegmentKind,
        values: &'a [u32],
    ) -> impl Iterator<Item = u32> + Clone + 'a {
        let mut original = values
            .iter()
            .copied()
            .filter(|asn| self.retained(*asn))
            .peekable();
        let set = matches!(kind, SegmentKind::Set | SegmentKind::ConfederationSet);
        let replacement = self.replacement.filter(|edit| {
            !set || original
                .clone()
                .any(|asn| edit.from.is_none_or(|from| from.get() == asn))
        });
        let mut inserted = false;
        core::iter::from_fn(move || {
            let Some(edit) = replacement else {
                return original.next();
            };
            if !set {
                return original.next().map(|asn| {
                    if edit.from.is_none_or(|from| from.get() == asn) {
                        edit.to.get()
                    } else {
                        asn
                    }
                });
            }
            // 集合替换是删除旧成员并有序插入新成员，不排序每条路径或保留重复的新成员。
            while original
                .peek()
                .is_some_and(|asn| edit.from.is_none_or(|from| from.get() == *asn))
            {
                original.next();
            }
            if !inserted && original.peek().is_none_or(|asn| *asn >= edit.to.get()) {
                inserted = true;
                if original.peek() == Some(&edit.to.get()) {
                    original.next();
                }
                Some(edit.to.get())
            } else {
                original.next()
            }
        })
    }
    fn segments(
        &self,
    ) -> impl Iterator<Item = (SegmentKind, impl Iterator<Item = u32> + Clone + '_)> + Clone + '_
    {
        let mut source = self.source.segments();
        let mut pending = None;
        let mut inserted = self.prepended.is_empty();
        core::iter::from_fn(move || {
            // 1. 删除集合/序列中的全部命中值，空段消失；保持幸存集合的独立边界。
            let segment = pending.take().or_else(|| {
                source.find(|(_, values)| values.iter().any(|value| self.retained(*value)))
            });
            if !inserted
                && segment.is_none_or(|(kind, _)| {
                    matches!(kind, SegmentKind::Sequence | SegmentKind::Set)
                })
            {
                inserted = true;
                pending = segment;
                return Some((SegmentKind::Sequence, self.prepended, &[][..]));
            }
            segment.map(|(kind, values)| (kind, &[][..], values))
        })
        .map(|(kind, prepended, original)| {
            (
                kind,
                prepended
                    .iter()
                    .map(|asn| asn.get())
                    .chain(self.mapped(kind, original)),
            )
        })
    }
    fn spans(&self) -> impl Iterator<Item = Span> + Clone + '_ {
        let mut segments = self.segments().peekable();
        let mut end = 0;
        core::iter::from_fn(move || {
            let (kind, values) = segments.next()?;
            end += values.count() as u32;
            // 2. 中间空段移除后新相邻的同类序列也合并；集合不能因此改变跳数或成员语义。
            if matches!(
                kind,
                SegmentKind::Sequence | SegmentKind::ConfederationSequence
            ) {
                while segments.peek().is_some_and(|(next, _)| *next == kind) {
                    end += segments.next().expect("peeked segment").1.count() as u32;
                }
            }
            Some(Span { kind, end })
        })
    }
    fn asns(&self) -> impl Iterator<Item = u32> + Clone + '_ {
        self.segments().flat_map(|(_, values)| values)
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
    /// 先排除有序集合中的 AS，替换幸存值，再在首个普通段前插入有序列表；不改变原始路径。
    /// # Parameters
    /// source 为原共享路径，excluded 必须排序去重，replacement 只读，prepended 的顺序和重复次数保留。
    /// # Returns
    /// 返回唯一规范结果；命中或无变化时只复用共享引用，无中间路径或长期政策缓存。
    /// # Errors
    /// 索引范围溢出或新结果捕获/驻留失败；原路径和索引保持有效。
    pub(crate) fn project(
        &mut self,
        source: &StoredPath,
        excluded: &[NonZeroU32],
        replacement: Option<&AsPathReplacement>,
        prepended: &[NonZeroU32],
    ) -> Result<StoredPath, OutOfMemory> {
        if excluded.is_empty() && replacement.is_none() && prepended.is_empty() {
            return Ok(source.clone());
        }
        let total = source
            .0
            .get()
            .asns
            .len()
            .checked_add(prepended.len())
            .ok_or(OutOfMemory)?;
        u32::try_from(total).map_err(|_| OutOfMemory)?;
        let view = Projection {
            source: source.0.get(),
            excluded,
            replacement,
            prepended,
        };
        if let Some(shared) = self.0.find_by(|stored| {
            view.spans()
                .cmp(stored.spans.iter().copied())
                .then_with(|| view.asns().cmp(stored.asns.iter().copied()))
        }) {
            return Ok(StoredPath(shared));
        }
        // 3. 只有不同的最终结果才按实际容量捕获；资源失败不能发布半条路径。
        let mut spans = Vec::new();
        let mut asns = Vec::new();
        spans
            .try_reserve_exact(view.spans().count())
            .map_err(|_| OutOfMemory)?;
        asns.try_reserve_exact(view.asns().count())
            .map_err(|_| OutOfMemory)?;
        spans.extend(view.spans());
        asns.extend(view.asns());
        self.0.intern(CanonicalPath { spans, asns }).map(StoredPath)
    }
    /// 驻留本机宣告的空路径；不构造伪造的邻居 UPDATE，共享和退休沿同一索引。
    /// # Errors
    /// 共享驻留节点分配失败。
    pub(crate) fn empty(&mut self) -> Result<StoredPath, OutOfMemory> {
        self.0
            .intern(CanonicalPath {
                spans: Vec::new(),
                asns: Vec::new(),
            })
            .map(StoredPath)
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
impl fmt::Display for StoredPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // AS-plain 沿规范路径流式写入；集合和联盟类型是正则可观察语义，不能只串联所有 ASN。
        for (index, (kind, asns)) in self.segments().enumerate() {
            if index != 0 {
                formatter.write_str(" ")?;
            }
            let (begin, end, separator) = match kind {
                SegmentKind::Sequence => ("", "", " "),
                SegmentKind::Set => ("{", "}", ","),
                SegmentKind::ConfederationSequence => ("(", ")", " "),
                SegmentKind::ConfederationSet => ("[", "]", ","),
            };
            formatter.write_str(begin)?;
            for (position, asn) in asns.iter().enumerate() {
                if position != 0 {
                    formatter.write_str(separator)?;
                }
                write!(formatter, "{asn}")?;
            }
            formatter.write_str(end)?;
        }
        Ok(())
    }
}
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
        super::emit_path_attributes(segments, Some(bytes), width, aggregator, auxiliary_partial)
    }
    /// None 只预检并返回精确尺寸；Some 写最终属性。成员间保留联盟段，外部剥离；同成员不加 AS。
    pub(crate) fn emit_export(
        &self,
        bytes: Option<&mut [u8]>,
        width: AsnWidth,
        kind: crate::PeerKind,
        local_as: u32,
        aggregator: Option<Aggregator>,
        partial: bool,
    ) -> Result<usize, PacketError> {
        let mut source = self.segments().filter(|(segment, _)| {
            kind != crate::PeerKind::External
                || matches!(segment, SegmentKind::Sequence | SegmentKind::Set)
        });
        let mut first = source.next();
        let prefix = if kind == crate::PeerKind::Internal {
            None
        } else {
            let segment = if kind == crate::PeerKind::Confederation {
                SegmentKind::ConfederationSequence
            } else {
                SegmentKind::Sequence
            };
            let values = if first.is_some_and(|(old, values)| old == segment && values.len() < 255)
            {
                first.take().expect("checked first segment").1
            } else {
                &[]
            };
            Some((
                segment,
                ExportAsns {
                    prefix: Some(local_as),
                    values,
                },
            ))
        };
        let rest = first.into_iter().chain(source).flat_map(|(segment, asns)| {
            asns.chunks(255).map(move |values| {
                (
                    segment,
                    ExportAsns {
                        prefix: None,
                        values,
                    },
                )
            })
        });
        super::emit_path_attributes(
            prefix.into_iter().chain(rest),
            bytes,
            width,
            aggregator,
            partial,
        )
    }
}

// 只借一个本地 AS 与原切片，精确计数后直接写最终段；避免 prepend 复制整条驻留路径。
#[derive(Clone)]
struct ExportAsns<'a> {
    prefix: Option<u32>,
    values: &'a [u32],
}
impl Iterator for ExportAsns<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        self.prefix.take().or_else(|| {
            let (first, rest) = self.values.split_first()?;
            self.values = rest;
            Some(*first)
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let count = self.values.len() + usize::from(self.prefix.is_some());
        (count, Some(count))
    }
}
impl ExactSizeIterator for ExportAsns<'_> {}

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
        assert_eq!(alloc::format!("{a}"), "65001 65002 65002 {70000,80000}");
        let confederation = capture(
            &mut pool,
            &[
                (Confed, &[65010, 65011]),
                (SegmentKind::ConfederationSet, &[65013, 65012]),
                (Seq, &[70000]),
            ],
        );
        assert_eq!(
            alloc::format!("{confederation}"),
            "(65010 65011) [65012,65013] 70000"
        );
        drop(confederation);
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
        // prepend 的借用查找须与 wire 捕获得到同一规范身份，且不动联盟前缀/集合边界。
        let prepend = [NonZeroU32::new(70000).unwrap(); 2];
        for segments in [
            vec![],
            vec![(Confed, &[65010][..])],
            vec![(Set, &[65001, 65002][..])],
            vec![(Confed, &[65010][..]), (Seq, &[65001][..])],
        ] {
            let source = capture(&mut pool, &segments);
            let before = pool.len();
            let edited = pool.project(&source, &[], None, &prepend).unwrap();
            let mut expected = segments.clone();
            let boundary = usize::from(segments.first().is_some_and(|(kind, _)| *kind == Confed));
            expected.insert(boundary, (Seq, &[70000, 70000]));
            let captured = capture(&mut pool, &expected);
            assert!(core::ptr::eq(edited.0.get(), captured.0.get()));
            assert_eq!(edited.hop_count(), source.hop_count() + 2);
            let repeated = pool.project(&source, &[], None, &prepend).unwrap();
            assert!(core::ptr::eq(edited.0.get(), repeated.0.get()));
            assert!(core::ptr::eq(
                source.0.get(),
                pool.project(&source, &[], None, &[]).unwrap().0.get()
            ));
            drop((edited, captured, repeated));
            assert_eq!(pool.len(), before);
        }
        // 删除会移除整个段并使两端序列相邻；不能只验证最终 ASN 列表而漏掉段/跳数身份。
        let mut check = |segments: &[(SegmentKind, &[u32])],
                         excluded: &[u32],
                         replacement: Option<AsPathReplacement>,
                         prepended: &[u32],
                         expected: &[(SegmentKind, &[u32])]| {
            let source = capture(&mut pool, segments);
            let before = pool.len();
            let excluded = excluded
                .iter()
                .map(|asn| NonZeroU32::new(*asn).unwrap())
                .collect::<Vec<_>>();
            let prepended = prepended
                .iter()
                .map(|asn| NonZeroU32::new(*asn).unwrap())
                .collect::<Vec<_>>();
            let edited = pool
                .project(&source, &excluded, replacement.as_ref(), &prepended)
                .unwrap();
            let captured = capture(&mut pool, expected);
            assert!(core::ptr::eq(edited.0.get(), captured.0.get()));
            let repeated = pool
                .project(&source, &excluded, replacement.as_ref(), &prepended)
                .unwrap();
            assert!(core::ptr::eq(edited.0.get(), repeated.0.get()));
            drop((edited, captured, repeated));
            assert_eq!(pool.len(), before);
        };
        check(
            &[
                (Seq, &[65001, 65002, 65002]),
                (Set, &[65003]),
                (Seq, &[65004]),
            ],
            &[65003],
            None,
            &[],
            &[(Seq, &[65001, 65002, 65002, 65004])],
        );
        check(
            &[(Seq, &[65001]), (Set, &[65003]), (Seq, &[65004])],
            &[65001, 65004],
            None,
            &[65001],
            &[(Seq, &[65001]), (Set, &[65003])],
        );
        check(
            &[(Confed, &[65010]), (Seq, &[65001]), (Set, &[70000, 80000])],
            &[65001, 70000],
            None,
            &[70000],
            &[(Confed, &[65010]), (Seq, &[70000]), (Set, &[80000])],
        );
        check(
            &[(Set, &[65001]), (Seq, &[65002]), (Set, &[65003])],
            &[65002],
            None,
            &[],
            &[(Set, &[65001]), (Set, &[65003])],
        );
        check(
            &[(Confed, &[65010]), (Seq, &[65001]), (Set, &[70000])],
            &[65001, 65010, 70000],
            None,
            &[],
            &[],
        );
        check(
            &[(Seq, &[65001, 65001])],
            &[70000],
            None,
            &[],
            &[(Seq, &[65001, 65001])],
        );
        let replace = |from: Option<u32>, to: u32| {
            Some(AsPathReplacement {
                from: from.map(|asn| NonZeroU32::new(asn).unwrap()),
                to: NonZeroU32::new(to).unwrap(),
            })
        };
        check(
            &[(Seq, &[65001, 65002, 65001])],
            &[],
            replace(Some(65001), 70000),
            &[],
            &[(Seq, &[70000, 65002, 70000])],
        );
        check(
            &[(Set, &[65001, 65002, 70000])],
            &[],
            replace(Some(65001), 70000),
            &[],
            &[(Set, &[65002, 70000])],
        );
        check(
            &[(Set, &[65001, 65002, 70000])],
            &[],
            replace(Some(70000), 1),
            &[],
            &[(Set, &[1, 65001, 65002])],
        );
        check(
            &[
                (Confed, &[65010, 65011]),
                (Seq, &[65001, 65002]),
                (Set, &[65001, 70000]),
            ],
            &[],
            replace(None, 80000),
            &[],
            &[
                (Confed, &[80000, 80000]),
                (Seq, &[80000, 80000]),
                (Set, &[80000]),
            ],
        );
        check(
            &[(Seq, &[65001, 65002])],
            &[65001],
            replace(Some(65001), 70000),
            &[65001],
            &[(Seq, &[65001, 65002])],
        );
        check(
            &[(Set, &[65001, 65002])],
            &[],
            replace(Some(65001), 65001),
            &[],
            &[(Set, &[65001, 65002])],
        );
        check(&[(Seq, &[65001])], &[65001], replace(None, 70000), &[], &[]);
        check(
            &[(Set, &[65001, 65002])],
            &[],
            replace(Some(70000), 80000),
            &[],
            &[(Set, &[65001, 65002])],
        );
        check(
            &[
                (SegmentKind::ConfederationSet, &[65001, 70000]),
                (Seq, &[65001]),
            ],
            &[],
            replace(Some(65001), 70000),
            &[65001],
            &[
                (SegmentKind::ConfederationSet, &[70000]),
                (Seq, &[65001, 70000]),
            ],
        );
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
        let original = capture(&mut pool, &[(Seq, &[70000; 90])]);
        let path = pool
            .project(
                &original,
                &[],
                None,
                &[NonZeroU32::new(65001).unwrap(); 510],
            )
            .unwrap();
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
