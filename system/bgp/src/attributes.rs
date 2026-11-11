//! 跨消息的路由属性内容 owner；路径单独驻留，NLRI/下一跳仍归各路由，不混入属性身份。
use crate::{
    Aggregator, Change, PathPool, ReceivedUpdate, StoredPath,
    residency::{Pool, Shared},
};
use alloc::vec::Vec;
use core::{cmp::Ordering, net::Ipv4Addr, num::NonZeroU32};
use fallible_tree::OutOfMemory;
use packet_wire::PacketError;
use packet_wire::bgp::attributes::{AsnWidth, Attribute, Value};
use router_protocol::bgp::{AsPathReplacement, CommunityEdit, MedAction, Origin, RouteAction};
mod communities;

/// 对不可变共享属性的借用编辑；所有集合来自已校验的规范配置，None 表示保持。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttributeChanges<'a> {
    pub origin: Option<Origin>,
    pub med: Option<MedAction>,
    /// 有序 prepend，不排序/去重；原始路径和其他共享组件不变。
    pub as_path_prepend: &'a [NonZeroU32],
    /// 已排序去重的删除集合，先删除再前置；只借用已校验配置。
    pub as_path_exclude: &'a [NonZeroU32],
    /// 只借用配置；按删除、替换、前置的顺序执行，不改变原始路径。
    pub as_path_replace: Option<&'a AsPathReplacement>,
    pub communities: Option<&'a CommunityEdit<u32>>,
    pub extended_communities: Option<&'a CommunityEdit<[u8; 8]>>,
    pub large_communities: Option<&'a CommunityEdit<[u8; 12]>>,
}
impl AttributeChanges<'_> {
    /// 已获准规则的只读投影；None 是缺省允许且没有编辑，Deny 不可进入此阶段。
    pub(crate) fn permitted(action: Option<&RouteAction>) -> AttributeChanges<'_> {
        match action {
            None => AttributeChanges::default(),
            Some(RouteAction::Permit {
                origin,
                med,
                communities,
                extended_communities,
                large_communities,
                as_path_prepend,
                as_path_exclude,
                as_path_replace,
                ..
            }) => AttributeChanges {
                origin: *origin,
                med: *med,
                as_path_prepend: as_path_prepend.as_deref().unwrap_or(&[]),
                as_path_exclude: as_path_exclude.as_deref().unwrap_or(&[]),
                as_path_replace: as_path_replace.as_deref(),
                communities: communities.as_deref(),
                extended_communities: extended_communities.as_deref(),
                large_communities: large_communities.as_deref(),
            },
            Some(RouteAction::Deny) => unreachable!("denied route has no attribute projection"),
        }
    }
    /// 仅包含标量的编辑；与集合编辑走同一投影路径，不拥有另一份属性值。
    pub const fn scalars(origin: Option<Origin>, med: Option<MedAction>) -> Self {
        Self {
            origin,
            med,
            as_path_prepend: &[],
            as_path_exclude: &[],
            as_path_replace: None,
            communities: None,
            extended_communities: None,
            large_communities: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Opaque {
    code: u8,
    data: Vec<u8>,
}
// 各可变长组件只持 Shared 引用；Clone 不分配，不复制列表或不透明 payload。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Contents {
    path: StoredPath,
    origin: u8,
    med: Option<u32>,
    local_preference: Option<u32>,
    atomic_aggregate: bool,
    aggregator: Option<Aggregator>,
    // OWNER: 支持的可传递属性（8/16/17/32）之 Partial，不含已由 Aggregator 拥有的位。
    // 丢掉这些位会在驻留/重新传播时把上游 Partial 清零；未知属性在不透明视图中强制 Partial。
    partial: u64,
    communities: Option<Shared<Vec<u32>>>,
    extended_communities: Option<Shared<Vec<[u8; 8]>>>,
    large_communities: Option<Shared<Vec<[u8; 12]>>>,
    originator: Option<Ipv4Addr>,
    cluster_list: Option<Shared<Vec<Ipv4Addr>>>,
    opaque: Option<Shared<Vec<Opaque>>>,
}
impl Contents {
    // 唯一排序 key 同时供驻留排序与借用投影查询；字段增减只在这里维护，不能另写近似的比较器。
    fn key(&self, origin: u8, med: Option<u32>) -> impl Ord + '_ {
        (
            (
                &self.path,
                origin,
                med,
                self.local_preference,
                self.atomic_aggregate,
                self.aggregator,
                self.partial,
            ),
            (
                &self.communities,
                &self.extended_communities,
                &self.large_communities,
                self.originator,
                &self.cluster_list,
                &self.opaque,
            ),
        )
    }
}
impl Ord for Contents {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key(self.origin, self.med)
            .cmp(&other.key(other.origin, other.med))
    }
}
impl PartialOrd for Contents {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
fn community_set<T: Copy + Ord>(
    values: impl ExactSizeIterator<Item = T>,
) -> Result<Vec<T>, OutOfMemory> {
    let count = values.len();
    let mut result = Vec::new();
    result.try_reserve_exact(count).map_err(|_| OutOfMemory)?;
    result.extend(values);
    result.sort_unstable();
    result.dedup();
    if result.len() != count {
        // 排序临时窗口随调用结束释放；驻留值不长期保留按恶意重复项数量扩大的容量。
        let mut packed = Vec::new();
        packed
            .try_reserve_exact(result.len())
            .map_err(|_| OutOfMemory)?;
        packed.extend_from_slice(&result);
        return Ok(packed);
    }
    Ok(result)
}

/// 一个 BGP actor 的完整属性/独立路径驻留表；索引及释放统一由 residency 拥有。
pub struct AttributePool {
    paths: PathPool,
    entries: Pool<Contents>,
    communities: Pool<Vec<u32>>,
    extended_communities: Pool<Vec<[u8; 8]>>,
    large_communities: Pool<Vec<[u8; 12]>>,
    // OWNER: 反射链/未知 payload 同样按内容共享；标量政策不复制它们，空组件不建立对象。
    cluster_lists: Pool<Vec<Ipv4Addr>>,
    opaque: Pool<Vec<Opaque>>,
}
impl AttributePool {
    /// 创建空表；任何分配失败都释放此前未发表的控制块。
    /// # Errors
    /// 路径表或属性表控制块分配失败。
    pub fn try_new() -> Result<Self, OutOfMemory> {
        Ok(Self {
            paths: PathPool::try_new()?,
            entries: Pool::try_new()?,
            communities: Pool::try_new()?,
            extended_communities: Pool::try_new()?,
            large_communities: Pool::try_new()?,
            cluster_lists: Pool::try_new()?,
            opaque: Pool::try_new()?,
        })
    }
    /// 仍被引用的不同完整属性数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// 是否没有任何完整属性引用。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 活跃属性引用的不同规范路径数量，用于内存/路由观测，不创建另一份统计状态。
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }
    /// 在同一规范池中投影政策；已有集合/属性不重新分配，未变化直接复用原属性，不修改原始接收。
    /// # Parameters
    /// source 为已捕获属性，changes 的集合须经配置校验排序/去重；未设置保持，MED Remove 保留“缺失”而非伪造零。
    /// # Errors
    /// 新集合、属性控制块或索引分配失败；共享组件和旧属性保持有效。
    pub fn project(
        &mut self,
        source: &RouteAttributes,
        changes: AttributeChanges<'_>,
    ) -> Result<RouteAttributes, OutOfMemory> {
        let value = source.0.get();
        let origin = changes.origin.map_or(value.origin, |origin| origin as u8);
        let med = match changes.med {
            None => value.med,
            Some(MedAction::Remove) => None,
            Some(MedAction::Set { value }) => Some(value),
        };
        if origin == value.origin
            && med == value.med
            && changes.as_path_prepend.is_empty()
            && changes.as_path_exclude.is_empty()
            && changes.as_path_replace.is_none()
            && changes.communities.is_none()
            && changes.extended_communities.is_none()
            && changes.large_communities.is_none()
        {
            return Ok(source.clone());
        }
        if changes.as_path_prepend.is_empty()
            && changes.as_path_exclude.is_empty()
            && changes.as_path_replace.is_none()
            && changes.communities.is_none()
            && changes.extended_communities.is_none()
            && changes.large_communities.is_none()
            && let Some(shared) = self.entries.find_by(|stored| {
                value
                    .key(origin, med)
                    .cmp(&stored.key(stored.origin, stored.med))
            })
        {
            return Ok(RouteAttributes(shared));
        }
        let mut projected = value.clone();
        projected.origin = origin;
        projected.med = med;
        if !changes.as_path_prepend.is_empty()
            || !changes.as_path_exclude.is_empty()
            || changes.as_path_replace.is_some()
        {
            projected.path = self.paths.project(
                &value.path,
                changes.as_path_exclude,
                changes.as_path_replace,
                changes.as_path_prepend,
            )?;
        }
        projected.communities = communities::project(
            &mut self.communities,
            &value.communities,
            changes.communities,
        )?;
        projected.extended_communities = communities::project(
            &mut self.extended_communities,
            &value.extended_communities,
            changes.extended_communities,
        )?;
        projected.large_communities = communities::project(
            &mut self.large_communities,
            &value.large_communities,
            changes.large_communities,
        )?;
        for (code, absent) in [
            (8, projected.communities.is_none()),
            (16, projected.extended_communities.is_none()),
            (32, projected.large_communities.is_none()),
        ] {
            if absent {
                projected.partial &= !(1u64 << code);
            }
        }
        if projected == *value {
            return Ok(source.clone());
        }
        if let Some(shared) = self.entries.find_by(|stored| projected.cmp(stored)) {
            return Ok(RouteAttributes(shared));
        }
        self.entries.intern(projected).map(RouteAttributes)
    }
    /// 本机 network 宣告的 IGP 起源与空路径；出口政策仍可编辑，不冒充收到的 UPDATE。
    /// # Errors
    /// 新共享组件/属性驻留分配失败，已有属性仍有效。
    pub(crate) fn originated(&mut self) -> Result<RouteAttributes, OutOfMemory> {
        self.local(0, None)
    }
    /// 其他 RIB 来源使用 INCOMPLETE 起源及实际度量；与本机/学习属性共享同一驻留池。
    /// # Errors
    /// 新共享组件/属性驻留分配失败。
    pub(crate) fn redistributed(&mut self, metric: u32) -> Result<RouteAttributes, OutOfMemory> {
        self.local(2, Some(metric))
    }
    fn local(&mut self, origin: u8, med: Option<u32>) -> Result<RouteAttributes, OutOfMemory> {
        self.entries
            .intern(Contents {
                path: self.paths.empty()?,
                origin,
                med,
                local_preference: None,
                atomic_aggregate: false,
                aggregator: None,
                partial: 0,
                communities: None,
                extended_communities: None,
                large_communities: None,
                originator: None,
                cluster_list: None,
                opaque: None,
            })
            .map(RouteAttributes)
    }
    /// 从已完整裁决的 UPDATE 捕获共享属性；仅撤回、EoR 或 TreatAsWithdraw 返回 None。
    /// # Errors
    /// 规范化或驻留分配失败；新候选自动释放，不改变已有属性/路径引用。
    pub fn intern(
        &mut self,
        update: &ReceivedUpdate<'_>,
    ) -> Result<Option<RouteAttributes>, OutOfMemory> {
        if !update
            .changes()
            .any(|change| matches!(change, Change::Announce { .. }))
        {
            return Ok(None);
        }
        let path = update
            .path()
            .expect("accepted announcement has a valid path");
        let mut contents = Contents {
            path: self.paths.intern(path)?,
            origin: 0,
            med: None,
            local_preference: None,
            atomic_aggregate: false,
            aggregator: update.aggregator(),
            partial: if path.auxiliary_partial() { 1 << 17 } else { 0 },
            communities: None,
            extended_communities: None,
            large_communities: None,
            originator: None,
            cluster_list: None,
            opaque: None,
        };
        // 1. 原属性序列不再含 AS_PATH/AGGREGATOR/AS4 辅助值；剩余值布局均不依赖 AS 宽度。
        let decoded = || {
            update.received_attributes().map(|attribute| {
                (
                    attribute,
                    attribute
                        .decode(AsnWidth::Four)
                        .expect("retained non-AS attribute prevalidated"),
                )
            })
        };
        let unknown = decoded()
            .filter(|(_, value)| matches!(value, Value::Unknown(_)))
            .count();
        let mut opaque = Vec::new();
        let mut cluster_list = Vec::new();
        opaque.try_reserve_exact(unknown).map_err(|_| OutOfMemory)?;
        let mut origin = None;
        for (attribute, value) in decoded() {
            if matches!(attribute.code, 8 | 16 | 32) && attribute.flags & 0x20 != 0 {
                contents.partial |= 1u64 << attribute.code;
            }
            match value {
                Value::Origin(value) => origin = Some(value),
                Value::MultiExitDiscriminator(value) => contents.med = Some(value),
                Value::LocalPreference(value) => contents.local_preference = Some(value),
                Value::AtomicAggregate => contents.atomic_aggregate = true,
                Value::Communities(bytes) => {
                    contents.communities = Some(
                        self.communities.intern(community_set(
                            bytes
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .copied()
                                .map(u32::from_be_bytes),
                        )?)?,
                    )
                }
                Value::ExtendedCommunities(bytes) => {
                    contents.extended_communities = Some(
                        self.extended_communities
                            .intern(community_set(bytes.as_chunks::<8>().0.iter().copied())?)?,
                    )
                }
                Value::LargeCommunities(bytes) => {
                    contents.large_communities = Some(
                        self.large_communities
                            .intern(community_set(bytes.as_chunks::<12>().0.iter().copied())?)?,
                    )
                }
                Value::OriginatorId(value) => contents.originator = Some(value),
                Value::ClusterList(bytes) => {
                    cluster_list
                        .try_reserve_exact(bytes.len() / 4)
                        .map_err(|_| OutOfMemory)?;
                    cluster_list
                        .extend(bytes.as_chunks::<4>().0.iter().copied().map(Ipv4Addr::from));
                }
                Value::Unknown(bytes) => {
                    let mut data = Vec::new();
                    data.try_reserve_exact(bytes.len())
                        .map_err(|_| OutOfMemory)?;
                    data.extend_from_slice(bytes);
                    opaque.push(Opaque {
                        code: attribute.code,
                        data,
                    });
                }
                Value::NextHop(_) | Value::MpReach(_) | Value::MpUnreach(_) => {}
                Value::AsPath(_) | Value::Aggregator { .. } => {
                    unreachable!("received AS attributes have separate effective accessors")
                }
            }
        }
        // 2. 团体是集合；反射链保留顺序和重复。未知属性仅按 code 排序，值不按猜测解释。
        contents.origin = origin.expect("accepted announcement has ORIGIN");
        opaque.sort_unstable_by_key(|attribute| attribute.code);
        if !cluster_list.is_empty() {
            contents.cluster_list = Some(self.cluster_lists.intern(cluster_list)?);
        }
        if !opaque.is_empty() {
            contents.opaque = Some(self.opaque.intern(opaque)?);
        }
        // 3. 全部捕获成功后才驻留；失败时 Contents::path 的引用自动退回，不留半个属性对象。
        self.entries
            .intern(contents)
            .map(|shared| Some(RouteAttributes(shared)))
    }
}

/// 可廉价克隆的完整属性引用，每个路由只持一个指针；Eq/Ord 仅为规范内容身份，不是最佳路径规则。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteAttributes(Shared<Contents>);
impl RouteAttributes {
    /// 生成本属性对象的路径/聚合器属性，保留所有适用的 Partial 元数据。
    /// 调用者仍须完成出口政策；此方法不生成整个 UPDATE 或执行本地 AS prepend。
    /// # Errors
    /// 非法 AS/联盟迁移、属性长度越界或容量不足；失败不修改输出。
    pub fn emit_path_attributes(
        &self,
        bytes: &mut [u8],
        width: AsnWidth,
    ) -> Result<usize, PacketError> {
        self.path()
            .emit_attributes(bytes, width, self.aggregator(), self.is_partial(17))
    }
    /// 独立驻留的有效 AS 路径，不包含收到的辅助路径副本。
    pub fn path(&self) -> &StoredPath {
        &self.0.get().path
    }
    /// 已验证的 ORIGIN：0/1/2。
    pub fn origin(&self) -> u8 {
        self.0.get().origin
    }
    /// 当前属性值的 MED，缺失和显式零保持可区分；比较时的缺省规则由选路 owner 处理。
    pub fn med(&self) -> Option<u32> {
        self.0.get().med
    }
    /// 原 LOCAL_PREF；eBGP 已在接收裁决中丢弃，不在此暗中套用入口政策。
    pub fn local_preference(&self) -> Option<u32> {
        self.0.get().local_preference
    }
    /// 是否携 ATOMIC_AGGREGATE。
    pub fn atomic_aggregate(&self) -> bool {
        self.0.get().atomic_aggregate
    }
    /// 完成 AS4 覆盖的聚合器及 Partial 元数据。
    pub fn aggregator(&self) -> Option<Aggregator> {
        self.0.get().aggregator
    }
    /// 排序去重的普通团体值。
    pub fn communities(&self) -> &[u32] {
        self.0
            .get()
            .communities
            .as_ref()
            .map_or(&[], |value| value.get().as_slice())
    }
    /// 排序去重的八字节扩展团体；边界传播筛选仍由出口政策负责。
    pub fn extended_communities(&self) -> &[[u8; 8]] {
        self.0
            .get()
            .extended_communities
            .as_ref()
            .map_or(&[], |value| value.get().as_slice())
    }
    /// 排序去重的十二字节大团体，重复值不进入驻留身份。
    pub fn large_communities(&self) -> &[[u8; 12]] {
        self.0
            .get()
            .large_communities
            .as_ref()
            .map_or(&[], |value| value.get().as_slice())
    }
    /// 原始反射起源标识。
    pub fn originator_id(&self) -> Option<Ipv4Addr> {
        self.0.get().originator
    }
    /// 按收到顺序保留的反射链，不能作为集合排序/去重。
    pub fn cluster_list(&self) -> &[Ipv4Addr] {
        self.0
            .get()
            .cluster_list
            .as_ref()
            .map_or(&[], |value| value.get().as_slice())
    }
    /// 传播时必须保留的 Partial；未知可传递属性按 RFC 4271 要求置位。
    pub fn is_partial(&self, code: u8) -> bool {
        if code == 7 {
            return self
                .aggregator()
                .is_some_and(|aggregator| aggregator.partial);
        }
        code < 64 && self.0.get().partial & (1u64 << code) != 0
            || self
                .0
                .get()
                .opaque
                .iter()
                .flat_map(|value| value.get())
                .any(|attribute| attribute.code == code)
    }
    /// 不解释的不透明可传递属性，按 code 排序，保留精确值并置 Partial；不含未知非可传递项。
    pub fn unknown_transitive(&self) -> impl Iterator<Item = Attribute<'_>> {
        self.0
            .get()
            .opaque
            .iter()
            .flat_map(|value| value.get())
            .map(|value| Attribute {
                flags: 0xe0,
                code: value.code,
                data: &value.data,
            })
    }
    /// 捕获本次管理查询的结构化属性，不复用为另一份可写路由表。
    /// # Errors
    /// 路径或团体输出分配失败；原共享属性保持有效。
    pub fn observe(&self) -> Result<router_protocol::bgp::PathAttributes, OutOfMemory> {
        fn values<T: Copy>(input: &[T]) -> Result<Vec<T>, OutOfMemory> {
            let mut output = Vec::new();
            output
                .try_reserve_exact(input.len())
                .map_err(|_| OutOfMemory)?;
            output.extend_from_slice(input);
            Ok(output)
        }
        let mut path = Vec::new();
        path.try_reserve_exact(self.path().segments().count())
            .map_err(|_| OutOfMemory)?;
        for (kind, asns) in self.path().segments() {
            path.push(router_protocol::bgp::PathSegment {
                kind: kind as u8,
                asns: values(asns)?,
            });
        }
        Ok(router_protocol::bgp::PathAttributes {
            path,
            origin: self.origin(),
            med: self.med(),
            local_preference: self.local_preference(),
            communities: values(self.communities())?,
            extended_communities: values(self.extended_communities())?,
            large_communities: values(self.large_communities())?,
        })
    }
}

#[cfg(test)]
mod tests;
