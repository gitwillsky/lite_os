#![no_std]
#![feature(allocator_ext)]

//! RIB 的不可变候选目录；路由按来源持有，属性和下一跳只按字典引用，不按前缀复制。
//! 冻结目录裁决前缀/优先级，resolution 编译下一跳及执行正文；授权、策略和发布协调由选路服务装配。
extern crate alloc;
use alloc::{sync::Arc, vec::Vec};
use prefix_index::{Address, Poptrie};

mod declaration;
mod policy;
pub use declaration::NextHop;
pub mod resolution;
pub mod sources;

/// 候选捕获或索引预备失败；不携带 Linux errno 或设备执行状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// 零来源/比较范围、重复来源，或字典引用越界。
    InvalidSource,
    /// 地址不是规范网络或前缀过长。
    InvalidPrefix,
    /// 内存不足。
    NoMemory,
    /// 记录/字典身份或精确权重超出可表达范围。
    Capacity,
}
impl From<prefix_index::Error> for Error {
    fn from(error: prefix_index::Error) -> Self {
        match error {
            prefix_index::Error::Invalid => Self::InvalidPrefix,
            prefix_index::Error::NoMemory => Self::NoMemory,
            prefix_index::Error::NoSpace => Self::Capacity,
        }
    }
}

/// 同一来源共享的选路属性；data 的领域含义由实际提供者定义。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Attributes<D> {
    /// 全局路由偏好/管理距离，较小优先。
    pub preference: u32,
    /// 同一比较范围内的代价，较小优先；不同范围的值不直接比较。
    pub metric: u32,
    /// 共享的领域属性，不复制到每条前缀。
    pub data: D,
}

/// 一条紧凑来源记录；属性和下一跳都属于同一个 Source 的字典。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route<A> {
    /// 规范网络地址。
    pub network: A,
    /// 网络前缀位数。
    pub length: u8,
    /// 从零开始的属性字典引用。
    pub attributes: u32,
    /// 从零开始的下一跳对象引用；解析/ECMP 属于下一跳 owner。
    pub next_hop: u32,
}

/// 一个生产者的不可变完整声明；来源更新替换整个对象，不原地修改其他快照。
pub struct Source<A, D, H, M = ()> {
    definition: Arc<SourceDefinition<H, M>>,
    routes: Vec<Route<A>>,
    attributes: Vec<Attributes<D>>,
}
// OWNER: 一次捕获的身份/元数据与路径字典不可变共享；策略投影不克隆任意 M/H，也不增加逐行引用。
struct SourceDefinition<H, M> {
    id: u64,
    domain: u64,
    metadata: M,
    next_hops: Vec<H>,
}
impl<A: Address, D, H, M> Source<A, D, H, M> {
    /// 接管来源声明，完整验证前缀/字典引用；没有逐条 Arc 或成员副本。
    /// # Parameters
    /// `id` 是装配层授予的非零稳定来源身份，不是路由声明可伪造的权限。
    /// `domain` 是 owner 授予的非零代价比较范围；不同范围的 metric 不可直接比较，等偏好先按范围稳定裁决。
    /// `metadata` 由授权 owner 冻结，不要求 Clone/Ord，也不参与声明字典去重。
    /// `attributes`/`next_hops` 为共享字典，捕获时删除无引用项并按完整值合并、重映射；data/成员身份由提供者的 Ord 定义。
    /// `routes` 的原顺序用于同源完全等价项的稳定裁决；D/H/M 的声明不能通过内部可变性在捕获后改变，实时可用性由查询谓词提供。
    /// # Returns
    /// 一份可由当前与候选目录共享的不可变来源。
    /// # Errors
    /// 身份/引用/前缀非法、数量超界，或 Arc 控制块分配失败；失败不发表来源。
    pub fn capture(
        id: u64,
        domain: u64,
        metadata: M,
        attributes: Vec<Attributes<D>>,
        next_hops: Vec<H>,
        mut routes: Vec<Route<A>>,
    ) -> Result<Arc<Self>, Error>
    where
        D: Ord,
        H: Ord,
    {
        if id == 0 || domain == 0 {
            return Err(Error::InvalidSource);
        }
        for count in [routes.len(), attributes.len(), next_hops.len()] {
            u32::try_from(count).map_err(|_| Error::Capacity)?;
        }
        for route in &routes {
            if route.length > A::BITS || route.network.masked(route.length) != route.network {
                return Err(Error::InvalidPrefix);
            }
            if route.attributes as usize >= attributes.len()
                || route.next_hop as usize >= next_hops.len()
            {
                return Err(Error::InvalidSource);
            }
        }
        // 字典只存实际引用的不同值；两份旧索引映射只在捕获时存在，不按前缀持久分配。
        let mut attribute_map = index_map(attributes.len())?;
        let mut next_hop_map = index_map(next_hops.len())?;
        for route in &routes {
            attribute_map[route.attributes as usize] = 0;
            next_hop_map[route.next_hop as usize] = 0;
        }
        let attributes = intern(attributes, &mut attribute_map)?;
        let next_hops = intern(next_hops, &mut next_hop_map)?;
        for route in &mut routes {
            route.attributes = attribute_map[route.attributes as usize];
            route.next_hop = next_hop_map[route.next_hop as usize];
        }
        let definition = Arc::try_new(SourceDefinition {
            id,
            domain,
            metadata,
            next_hops,
        })
        .map_err(|_| Error::NoMemory)?;
        Arc::try_new(Self {
            definition,
            routes,
            attributes,
        })
        .map_err(|_| Error::NoMemory)
    }
    // 冷策略投影只拥有变更后的属性/行，下一跳字典与原声明共享；拒绝分配失败时改变原始来源。
    fn project_attributes(
        &self,
        attributes: Vec<Attributes<D>>,
        routes: Vec<Route<A>>,
    ) -> Result<Arc<Self>, Error> {
        u32::try_from(attributes.len()).map_err(|_| Error::Capacity)?;
        for route in &routes {
            if route.length > A::BITS
                || route.network.masked(route.length) != route.network
                || route.attributes as usize >= attributes.len()
                || route.next_hop as usize >= self.definition.next_hops.len()
            {
                return Err(Error::InvalidSource);
            }
        }
        Arc::try_new(Self {
            definition: self.definition.clone(),
            routes,
            attributes,
        })
        .map_err(|_| Error::NoMemory)
    }
    /// 装配层授予的来源身份。
    pub fn id(&self) -> u64 {
        self.definition.id
    }
    /// 由授权 owner 赋予、与声明一起冻结的来源元数据；不由路由正文决定权限。
    pub fn metadata(&self) -> &M {
        &self.definition.metadata
    }
    /// 原始声明顺序的全部前缀，不授予修改权。
    pub fn routes(&self) -> impl ExactSizeIterator<Item = &Route<A>> {
        self.routes.iter()
    }
    /// 来源的共享属性。
    pub fn attributes(&self) -> &[Attributes<D>] {
        &self.attributes
    }
    /// 来源的共享下一跳对象；不复制成员。
    pub fn next_hops(&self) -> &[H] {
        &self.definition.next_hops
    }
}

fn index_map(count: usize) -> Result<Vec<u32>, Error> {
    let mut map = Vec::new();
    map.try_reserve_exact(count).map_err(|_| Error::NoMemory)?;
    map.resize(count, u32::MAX);
    Ok(map)
}
fn intern<T: Ord>(values: Vec<T>, map: &mut [u32]) -> Result<Vec<T>, Error> {
    let used = map.iter().filter(|index| **index != u32::MAX).count();
    let mut indexed = Vec::new();
    indexed
        .try_reserve_exact(used)
        .map_err(|_| Error::NoMemory)?;
    indexed.extend(
        values
            .into_iter()
            .enumerate()
            .filter(|(index, _)| map[*index] != u32::MAX),
    );
    indexed.sort_unstable_by(|(_, a), (_, b)| a.cmp(b));
    let distinct = indexed.chunk_by(|(_, a), (_, b)| a == b).count();
    let mut result = Vec::new();
    result
        .try_reserve_exact(distinct)
        .map_err(|_| Error::NoMemory)?;
    for (previous, value) in indexed {
        if result.last() != Some(&value) {
            result.push(value);
        }
        map[previous] = result.len() as u32 - 1;
    }
    Ok(result)
}

#[derive(Clone, Copy)]
struct Reference {
    source: u32,
    route: u32,
}

/// 对同一冻结来源的借用投影；目录保活源数组，不分配、增加逐路由引用或隐藏下一跳失效。
pub struct Candidate<'a, A, D, H, M = ()> {
    source: &'a Source<A, D, H, M>,
    route: &'a Route<A>,
}
impl<A: Address, D, H, M> Candidate<'_, A, D, H, M> {
    /// 真实来源身份。
    pub fn source(&self) -> u64 {
        self.source.definition.id
    }
    /// 所属来源的可信元数据；没有逐行副本。
    pub fn metadata(&self) -> &M {
        &self.source.definition.metadata
    }
    /// 规范前缀及字典引用。
    pub fn route(&self) -> &Route<A> {
        self.route
    }
    /// 所属来源的共享属性。
    pub fn attributes(&self) -> &Attributes<D> {
        &self.source.attributes[self.route.attributes as usize]
    }
    /// 所属来源的下一跳对象，后续解析不得用别的来源字典解释此引用。
    pub fn next_hop(&self) -> &H {
        &self.source.definition.next_hops[self.route.next_hop as usize]
    }
}

/// 一份路由实例/地址族的冻结候选目录；只复制八字节来源/行引用，实际声明继续共享。
/// 目录代际的 Arc 只在管理快照/预备时克隆，查找和行投影不增减引用计数；上层在发布锁外退休旧代际。
pub struct Catalogue<A, D, H, M = ()> {
    sources: Vec<Arc<Source<A, D, H, M>>>,
    references: Vec<Reference>,
    index: Poptrie<A>,
}
impl<A: Address, D, H, M> Catalogue<A, D, H, M> {
    /// 从已捕获的来源预备完整目录，不复制属性/成员或逐路由声明。
    /// # Parameters
    /// `sources` 每个身份只出现一次，且属于同一实例/地址族；权限和实例选择由上层决定。
    /// # Returns
    /// 前缀有序、同前缀候选按偏好/比较范围/代价/稳定来源排序的只读目录。
    /// # Errors
    /// 重复来源、容量超界或分配失败；失败不改变原目录。
    pub fn build(mut sources: Vec<Arc<Source<A, D, H, M>>>) -> Result<Arc<Self>, Error> {
        u32::try_from(sources.len()).map_err(|_| Error::Capacity)?;
        sources.sort_unstable_by_key(|source| source.definition.id);
        if sources.windows(2).any(|pair| pair[0].id() == pair[1].id()) {
            return Err(Error::InvalidSource);
        }
        sources.retain(|source| !source.routes.is_empty());
        let count = sources.iter().try_fold(0usize, |total, source| {
            total
                .checked_add(source.routes.len())
                .ok_or(Error::Capacity)
        })?;
        u32::try_from(count).map_err(|_| Error::Capacity)?;
        let mut references = Vec::new();
        references
            .try_reserve_exact(count)
            .map_err(|_| Error::NoMemory)?;
        for (source, data) in sources.iter().enumerate() {
            references.extend((0..data.routes.len()).map(|route| Reference {
                source: source as u32,
                route: route as u32,
            }));
        }
        references.sort_unstable_by_key(|reference| {
            let source = &sources[reference.source as usize];
            let route = &source.routes[reference.route as usize];
            let attributes = &source.attributes[route.attributes as usize];
            (
                route.network,
                route.length,
                attributes.preference,
                source.definition.domain,
                attributes.metric,
                source.definition.id,
                reference.route,
            )
        });
        let index = Poptrie::build(references.iter().map(|reference| {
            let route = &sources[reference.source as usize].routes[reference.route as usize];
            (route.network, route.length)
        }))?;
        Arc::try_new(Self {
            sources,
            references,
            index,
        })
        .map_err(|_| Error::NoMemory)
    }
    /// 全部来源的声明条数，包含未被选中的候选。
    pub fn len(&self) -> usize {
        self.references.len()
    }
    /// 是否没有候选。
    pub fn is_empty(&self) -> bool {
        self.references.is_empty()
    }
    /// 各来源只被目录保活一次；只读管理查询不取得源数据写权。
    pub fn sources(&self) -> &[Arc<Source<A, D, H, M>>] {
        &self.sources
    }
    fn project(&self, reference: Reference) -> Candidate<'_, A, D, H, M> {
        let source = &self.sources[reference.source as usize];
        Candidate {
            source,
            route: &source.routes[reference.route as usize],
        }
    }
    /// 按最长前缀、偏好、比较范围、代价、稳定来源身份及声明顺序借用候选；不跳过未解析对象。
    /// # Parameters
    /// `address` 是要查找的下一跳/目的地址；解析和可用性由调用者基于一致事实裁决。
    /// # Returns
    /// 无分配的候选迭代器；不可用的窄前缀可以继续考察父前缀。
    pub fn candidates(&self, address: A) -> impl Iterator<Item = Candidate<'_, A, D, H, M>> {
        self.index.matching(address).flat_map(|prefix| {
            self.references[prefix.range()]
                .iter()
                .map(|reference| self.project(*reference))
        })
    }
    /// 用同一解析/事实谓词选择最长可用前缀的首选声明。
    /// # Returns
    /// 没有可用候选返回 None；不伪造递归已解析或已经安装 FIB。
    pub fn lookup(
        &self,
        address: A,
        mut eligible: impl FnMut(&Candidate<'_, A, D, H, M>) -> bool,
    ) -> Option<Candidate<'_, A, D, H, M>> {
        self.candidates(address)
            .find(|candidate| eligible(candidate))
    }
    /// 为每个精确前缀选择首个可用候选；ECMP 合并和下一跳解析由服务在同一冻结事实内完成。
    pub fn selected(
        &self,
        mut eligible: impl FnMut(&Candidate<'_, A, D, H, M>) -> bool,
    ) -> impl Iterator<Item = Candidate<'_, A, D, H, M>> {
        self.references
            .chunk_by(|a, b| {
                let a = self.project(*a);
                let b = self.project(*b);
                (a.route.network, a.route.length) == (b.route.network, b.route.length)
            })
            .filter_map(move |rows| {
                rows.iter()
                    .map(|row| self.project(*row))
                    .find(|candidate| eligible(candidate))
            })
    }
    /// 预备一个来源的完整替换；空声明撤销该来源，原目录继续有效直到上层发布新目录。
    /// # Errors
    /// 容量/索引失败或资源不足；失败不改变当前来源集合。
    pub fn prepare_replace(
        self: &Arc<Self>,
        replacement: Arc<Source<A, D, H, M>>,
    ) -> Result<Arc<Self>, Error> {
        if replacement.routes.is_empty() {
            return self.prepare_remove(replacement.definition.id);
        }
        if self
            .sources
            .binary_search_by_key(&replacement.definition.id, |source| source.definition.id)
            .is_ok_and(|index| Arc::ptr_eq(&self.sources[index], &replacement))
        {
            return Ok(self.clone());
        }
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(self.sources.len().checked_add(1).ok_or(Error::Capacity)?)
            .map_err(|_| Error::NoMemory)?;
        sources.extend(
            self.sources
                .iter()
                .filter(|source| source.definition.id != replacement.definition.id)
                .cloned(),
        );
        sources.push(replacement);
        Self::build(sources)
    }
    /// 按真实生产者寿命预备撤销其全部声明；不删除其他来源的相同前缀。
    /// # Errors
    /// 分配/索引失败；当前目录及读者保持原状。
    pub fn prepare_remove(self: &Arc<Self>, source_id: u64) -> Result<Arc<Self>, Error> {
        if self
            .sources
            .binary_search_by_key(&source_id, |source| source.definition.id)
            .is_err()
        {
            return Ok(self.clone());
        }
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(self.sources.len())
            .map_err(|_| Error::NoMemory)?;
        sources.extend(
            self.sources
                .iter()
                .filter(|source| source.definition.id != source_id)
                .cloned(),
        );
        Self::build(sources)
    }
}

#[cfg(test)]
mod tests;
