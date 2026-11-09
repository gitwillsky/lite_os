#![no_std]

//! 六位 stride 的 Poptrie（SIGCOMM 2015）：vector/leafvec + rank，叶子引用前缀身份而不是复制路由。
//! 调用者拥有排序的实际记录；本索引只持前缀身份、引用数和连续候选区间，不解释路由或服务状态。

extern crate alloc;

/// 索引预备失败；没有服务、协议或 Linux errno 语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// 前缀未规范化或长度超出地址宽度。
    Invalid,
    /// 分配失败。
    NoMemory,
    /// 前缀身份或记录区间超出 u32 容量。
    NoSpace,
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for u32 {}
    impl Sealed for u128 {}
}
use alloc::vec::Vec;

/// 索引支持的固定宽地址；仅 u32/u128 可实现，确保 stride 与位移始终有界。
pub trait Address: sealed::Sealed + Copy + Ord {
    /// 地址位数。
    const BITS: u8;
    /// 全零地址。
    fn zero() -> Self;
    /// 清除 length 之后的位；调用者保证 length 不超过 BITS。
    fn masked(self, length: u8) -> Self;
    /// 读取有界的当前 stride；offset+width 不超过 BITS。
    fn stride(self, offset: u8, width: u8) -> usize;
    /// 为有界 stride 填写 slot，保留此前网络位。
    fn slot(self, offset: u8, width: u8, slot: usize) -> Self;
}

macro_rules! address {
    ($type:ty) => {
        impl Address for $type {
            const BITS: u8 = <$type>::BITS as u8;
            fn zero() -> Self {
                0
            }
            fn masked(self, length: u8) -> Self {
                self & <$type>::MAX
                    .checked_shl(<$type>::BITS - u32::from(length))
                    .unwrap_or(0)
            }
            fn stride(self, offset: u8, width: u8) -> usize {
                ((self >> (<$type>::BITS - u32::from(offset + width))) & ((1 << width) - 1))
                    as usize
            }
            fn slot(self, offset: u8, width: u8, slot: usize) -> Self {
                self.masked(offset)
                    | ((slot as Self) << (<$type>::BITS - u32::from(offset + width)))
            }
        }
    };
}
address!(u32);
address!(u128);

/// 一个可读取的结构前缀；引用数和区间只由所属索引维护。
#[derive(Clone, Copy)]
pub struct Prefix<A> {
    /// 已规范化的网络地址。
    pub network: A,
    /// 前缀位数。
    pub length: u8,
    // OWNER: 调用者的同一记录/索引事务维护实际候选数；零表示仍可复用的结构前缀，不是路由。
    references: u32,
    // 与引用数构成调用者的同一连续候选区间；没有起点就必须重查全表才能找到候选。
    start: u32,
    parent: u32,
}
impl<A> Prefix<A> {
    /// 同一排序的实际 alias 连续区间；范围只由 build/recount/add/remove 与引用数共同维护。
    pub fn range(&self) -> core::ops::Range<usize> {
        self.start as usize..self.start as usize + self.references as usize
    }
}
#[derive(Clone, Copy, Default)]
struct Node {
    vector: u64,
    leafvec: u64,
    base0: u32,
    base1: u32,
}
#[derive(Clone, Copy, Default)]
struct Branch {
    start: u32,
    end: u32,
    inherited: u32,
}

/// 冷编译、热 rank 查找的唯一前缀索引；没有地址结果 cache、锁或原始指针。
pub struct Poptrie<A> {
    prefixes: Vec<Prefix<A>>,
    nodes: Vec<Node>,
    leaves: Vec<u32>,
}
/// 预备的一次引用增加；只能在原索引尚未修改时向该索引提交。
pub enum Addition<A> {
    /// 复用已有前缀身份，不重新编译结构。
    Existing(usize),
    /// 包含新结构的完整预备索引。
    Rebuilt(Poptrie<A>),
}

impl<A: Address> Default for Poptrie<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Address> Poptrie<A> {
    /// 构造空索引，不分配。
    pub fn new() -> Self {
        Self {
            prefixes: Vec::new(),
            nodes: Vec::new(),
            leaves: Vec::new(),
        }
    }
    /// 从实际记录集预备新布局；重复前缀合并为引用数，区间按 (network, length) 顺序构建。
    /// # Parameters
    /// `entries` 给出每条实际记录的网络地址与前缀长度；调用者的记录须采用相同的前缀排序。
    /// # Returns
    /// 独占前缀和位图的索引，不保存记录 payload。
    /// # Errors
    /// 前缀错误、u32 身份/区间溢出或分配失败。
    pub fn build(entries: impl Iterator<Item = (A, u8)>) -> Result<Self, Error> {
        Self::build_counted(entries.map(|(network, length)| (network, length, 1)))
    }
    fn build_counted(entries: impl Iterator<Item = (A, u8, u32)>) -> Result<Self, Error> {
        let mut result = Self::new();
        for (network, length, references) in entries {
            if length > A::BITS || network.masked(length) != network || references == 0 {
                return Err(Error::Invalid);
            }
            if result.prefixes.len() == u32::MAX as usize {
                return Err(Error::NoSpace);
            }
            result
                .prefixes
                .try_reserve(1)
                .map_err(|_| Error::NoMemory)?;
            result.prefixes.push(Prefix {
                network,
                length,
                references,
                start: 0,
                parent: 0,
            });
        }
        result
            .prefixes
            .sort_unstable_by_key(|prefix| (prefix.network, prefix.length));
        // 1. 原地合并重复前缀；先检查总数，不能在 dedup 回调里忽略引用计数溢出。
        let mut previous = None;
        let mut references = 0u32;
        for prefix in &result.prefixes {
            let key = (prefix.network, prefix.length);
            if previous != Some(key) {
                references = 0;
                previous = Some(key);
            }
            references = references
                .checked_add(prefix.references)
                .ok_or(Error::NoSpace)?;
        }
        result.prefixes.dedup_by(|next, previous| {
            if (next.network, next.length) != (previous.network, previous.length) {
                return false;
            }
            previous.references += next.references;
            true
        });
        if result.prefixes.is_empty() {
            return Ok(result);
        }
        let mut start = 0u32;
        for prefix in &mut result.prefixes {
            prefix.start = start;
            start = start.checked_add(prefix.references).ok_or(Error::NoSpace)?;
        }
        // 2. 有序前缀的祖先链；IPv4/IPv6 共用同一算法，深度由地址位数限制。
        let mut ancestors = [0u32; 129];
        let mut depth = 0usize;
        for index in 0..result.prefixes.len() {
            let prefix = result.prefixes[index];
            while depth != 0 {
                let parent = result.prefixes[(ancestors[depth - 1] - 1) as usize];
                if parent.length < prefix.length
                    && prefix.network.masked(parent.length) == parent.network
                {
                    break;
                }
                depth -= 1;
            }
            result.prefixes[index].parent = if depth == 0 { 0 } else { ancestors[depth - 1] };
            ancestors[depth] = index as u32 + 1;
            depth += 1;
        }
        // 3. 连续子节点/压缩叶 run 均在发布之前准备；失败只丢弃尚未发布的 result。
        result.nodes.try_reserve(1).map_err(|_| Error::NoMemory)?;
        result.nodes.push(Node::default());
        result.nodes[0] = result.compile(A::zero(), 0, 0, result.prefixes.len(), 0)?;
        Ok(result)
    }
    fn compile(
        &mut self,
        base: A,
        offset: u8,
        start: usize,
        end: usize,
        mut inherited: u32,
    ) -> Result<Node, Error> {
        let width = 6.min(A::BITS - offset);
        let slots = 1usize << width;
        let mut branches = [Branch::default(); 64];
        let mut vector = 0u64;
        let mut cursor = start;
        for (slot, branch) in branches[..slots].iter_mut().enumerate() {
            let key = base.slot(offset, width, slot);
            while inherited != 0 {
                let prefix = self.prefixes[(inherited - 1) as usize];
                if key.masked(prefix.length) == prefix.network {
                    break;
                }
                inherited = prefix.parent;
            }
            let mut first = cursor;
            while cursor < end && self.prefixes[cursor].network.stride(offset, width) == slot {
                cursor += 1;
            }
            while first < cursor && self.prefixes[first].length <= offset + width {
                inherited = first as u32 + 1;
                first += 1;
            }
            *branch = Branch {
                start: first as u32,
                end: cursor as u32,
                inherited,
            };
            if first != cursor {
                vector |= 1u64 << slot;
            }
        }
        let children = vector.count_ones() as usize;
        let base1 = u32::try_from(self.nodes.len()).map_err(|_| Error::NoSpace)?;
        let next = self
            .nodes
            .len()
            .checked_add(children)
            .filter(|next| *next <= u32::MAX as usize)
            .ok_or(Error::NoSpace)?;
        self.nodes
            .try_reserve(children)
            .map_err(|_| Error::NoMemory)?;
        self.nodes.resize(next, Node::default());
        let base0 = u32::try_from(self.leaves.len()).map_err(|_| Error::NoSpace)?;
        self.leaves
            .try_reserve(slots)
            .map_err(|_| Error::NoMemory)?;
        let mut leafvec = 0;
        let mut previous = None;
        // 叶 run 先连续存放；子树递归追加自己的叶，不能把当前节点的叶段交错。
        for (slot, branch) in branches[..slots].iter().enumerate() {
            if branch.start == branch.end && previous != Some(branch.inherited) {
                if self.leaves.len() == u32::MAX as usize {
                    return Err(Error::NoSpace);
                }
                self.leaves.push(branch.inherited);
                leafvec |= 1u64 << slot;
                previous = Some(branch.inherited);
            }
        }
        let mut child = base1 as usize;
        for (slot, branch) in branches[..slots].iter().enumerate() {
            if branch.start != branch.end {
                let node = self.compile(
                    base.slot(offset, width, slot),
                    offset + width,
                    branch.start as usize,
                    branch.end as usize,
                    branch.inherited,
                )?;
                self.nodes[child] = node;
                child += 1;
            }
        }
        Ok(Node {
            vector,
            leafvec,
            base0,
            base1,
        })
    }
    fn identity(&self, address: A) -> u32 {
        if self.nodes.is_empty() {
            return 0;
        }
        let mut index = 0;
        let mut offset = 0;
        loop {
            let node = self.nodes[index];
            let width = 6.min(A::BITS - offset);
            let slot = address.stride(offset, width);
            let bit = 1u64 << slot;
            // inclusive mask 不能在 slot=63 时移位 64。
            let through = u64::MAX >> (63 - slot);
            if node.vector & bit == 0 {
                return self.leaves
                    [node.base0 as usize + (node.leafvec & through).count_ones() as usize - 1];
            }
            index = node.base1 as usize + (node.vector & through).count_ones() as usize - 1;
            offset += width;
        }
    }
    /// 当前索引的结构前缀；身份为本切片下标，只在同一未修改索引内有效。
    /// 零引用结构可能保留供恢复使用；调用者不得把下标当作全局路由身份。
    pub fn prefixes(&self) -> &[Prefix<A>] {
        &self.prefixes
    }
    fn identities(&self, address: A) -> impl Clone + Iterator<Item = u32> {
        core::iter::successors(Some(self.identity(address)), |identity| {
            (*identity != 0).then(|| self.prefixes[(*identity - 1) as usize].parent)
        })
        .take_while(|identity| *identity != 0)
    }
    /// 从最窄匹配沿祖先退回，返回非零引用前缀在 prefixes 中的下标；不分配。
    /// 下标只供同一索引的旁路元数据使用，索引重建/修改后必须一起重建元数据。
    pub fn matching_indices(&self, address: A) -> impl Clone + Iterator<Item = usize> {
        self.identities(address).filter_map(|identity| {
            let index = (identity - 1) as usize;
            (self.prefixes[index].references != 0).then_some(index)
        })
    }
    /// 从最窄匹配沿祖先退回，跳过已退休 alias 的结构节点；不扫描无关前缀。
    pub fn matching(&self, address: A) -> impl Clone + Iterator<Item = &Prefix<A>> {
        self.identities(address).filter_map(|identity| {
            let prefix = &self.prefixes[(identity - 1) as usize];
            (prefix.references != 0).then_some(prefix)
        })
    }
    fn position(&self, network: A, length: u8) -> Result<usize, usize> {
        self.prefixes
            .binary_search_by_key(&(network, length), |prefix| (prefix.network, prefix.length))
    }
    /// 查询精确前缀对应的连续记录范围；结构不存在返回 None。
    pub fn range_for(&self, network: A, length: u8) -> Option<core::ops::Range<usize>> {
        self.position(network, length)
            .ok()
            .map(|index| self.prefixes[index].range())
    }
    /// 在原索引未变时预备一条记录；失败不改变原区间或引用数。
    /// # Errors
    /// 非法前缀、容量溢出或分配失败。
    pub fn prepare_add(&self, network: A, length: u8) -> Result<Addition<A>, Error> {
        if self.prefixes.last().is_some_and(|last| {
            u64::from(last.start) + u64::from(last.references) >= u64::from(u32::MAX)
        }) {
            return Err(Error::NoSpace);
        }
        if let Ok(index) = self.position(network, length) {
            self.prefixes[index]
                .references
                .checked_add(1)
                .ok_or(Error::NoSpace)?;
            return Ok(Addition::Existing(index));
        }
        Self::build_counted(
            self.prefixes
                .iter()
                .filter(|prefix| prefix.references != 0)
                .map(|prefix| (prefix.network, prefix.length, prefix.references))
                .chain(core::iter::once((network, length, 1))),
        )
        .map(Addition::Rebuilt)
    }
    /// 提交刚为本索引预备的增加，不分配；调用者须在同一事务更新实际记录。
    /// # Panics
    /// 预备结果来自其他索引或原索引已变化时，不能保证区间正确；不得这样使用。
    pub fn add(&mut self, prepared: Addition<A>) {
        match prepared {
            Addition::Existing(index) => {
                self.prefixes[index].references += 1;
                for prefix in &mut self.prefixes[index + 1..] {
                    prefix.start += 1;
                }
            }
            Addition::Rebuilt(next) => *self = next,
        }
    }
    /// 退休精确前缀的一条实际引用，不分配或重编译；调用者同步退休对应记录。
    /// # Panics
    /// 前缀不存在或已经没有实际引用。
    pub fn remove(&mut self, network: A, length: u8) {
        let index = self
            .position(network, length)
            .expect("actual route owns an indexed prefix");
        self.prefixes[index].references = self.prefixes[index]
            .references
            .checked_sub(1)
            .expect("route reference retired twice");
        for prefix in &mut self.prefixes[index + 1..] {
            prefix.start -= 1;
        }
    }
    /// 按相同 prefix 顺序重计数/区间，不分配或改形状；热拔出后的候选只来自实际 alias。
    pub fn recount(&mut self, entries: impl Iterator<Item = (A, u8)>) {
        let mut entries = entries.peekable();
        let mut start = 0u32;
        for prefix in &mut self.prefixes {
            prefix.start = start;
            prefix.references = 0;
            while entries
                .peek()
                .is_some_and(|key| *key == (prefix.network, prefix.length))
            {
                entries.next();
                prefix.references += 1;
                start += 1;
            }
        }
        assert!(
            entries.next().is_none(),
            "sorted actual aliases must be covered by the prepared index"
        );
        if start == 0 {
            *self = Self::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{cmp::Reverse, fmt::Debug};

    fn check<A: Address + Debug>(prefixes: &[(A, u8)], queries: &[A]) {
        let trie = Poptrie::build(prefixes.iter().copied()).unwrap();
        let mut ordered = prefixes.to_vec();
        ordered.sort_unstable();
        for prefix in &trie.prefixes {
            let key = (prefix.network, prefix.length);
            let start = ordered.partition_point(|entry| *entry < key);
            let end = ordered.partition_point(|entry| *entry <= key);
            assert_eq!(prefix.range(), start..end);
        }
        for &address in queries {
            let mut reference: Vec<_> = prefixes
                .iter()
                .copied()
                .filter(|(network, length)| address.masked(*length) == *network)
                .collect();
            reference.sort_unstable_by_key(|(_, length)| Reverse(*length));
            reference.dedup();
            let found: Vec<_> = trie
                .matching(address)
                .map(|prefix| (prefix.network, prefix.length))
                .collect();
            assert_eq!(found, reference, "query: {address:?}");
        }
    }

    #[test]
    fn compressed_rank_and_prefix_chains_match_linear_ipv4_and_ipv6_oracles() {
        let mut state = 0x37b1_45e2_19ab_80cdu128;
        let mut next = || {
            state = state
                .wrapping_mul(0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645)
                .wrapping_add(1);
            state
        };
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        for length in 0..=128u8 {
            ipv6.push((0u128, length));
            ipv6.push((u128::MAX.masked(length), length));
            if length <= 32 {
                ipv4.push((0u32, length));
                ipv4.push((u32::MAX.masked(length), length));
            }
        }
        for _ in 0..2048 {
            let raw = next();
            let length = (next() % 129) as u8;
            ipv6.push((raw.masked(length), length));
            let length = (next() % 33) as u8;
            ipv4.push(((raw as u32).masked(length), length));
        }
        let queries: Vec<_> = (0..1024).map(|_| next()).chain([0, u128::MAX]).collect();
        check(&ipv6, &queries);
        check(
            &ipv4,
            &queries
                .iter()
                .map(|query| *query as u32)
                .collect::<Vec<_>>(),
        );
        assert!(matches!(
            Poptrie::build(core::iter::once((1u32, 0))),
            Err(Error::Invalid)
        ));
        assert!(matches!(
            Poptrie::build(core::iter::once((0u32, 33))),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn alias_retirement_reuse_and_recount_preserve_ancestors_without_rebuilding() {
        let mut trie = Poptrie::build(
            [
                (0u32, 0),
                (0xfc00_0000, 6),
                (0xfc00_0000, 6),
                (u32::MAX, 32),
            ]
            .into_iter(),
        )
        .unwrap();
        let lengths = |trie: &Poptrie<u32>| {
            trie.matching(u32::MAX)
                .map(|prefix| prefix.length)
                .collect::<Vec<_>>()
        };
        let nodes = trie.nodes.as_ptr();
        trie.remove(0xfc00_0000, 6);
        assert_eq!(lengths(&trie), [32, 6, 0]);
        trie.remove(0xfc00_0000, 6);
        assert_eq!(lengths(&trie), [32, 0]);
        trie.remove(u32::MAX, 32);
        assert_eq!(lengths(&trie), [0]);
        let restored = trie.prepare_add(0xfc00_0000, 6).unwrap();
        assert!(matches!(restored, Addition::Existing(_)));
        trie.add(restored);
        assert_eq!(lengths(&trie), [6, 0]);
        assert_eq!(trie.nodes.as_ptr(), nodes);
        trie.recount(core::iter::once((u32::MAX, 32)));
        assert_eq!(lengths(&trie), [32]);
        trie.recount(core::iter::empty());
        assert_eq!(
            (
                trie.nodes.capacity(),
                trie.leaves.capacity(),
                trie.prefixes.capacity()
            ),
            (0, 0, 0)
        );
    }
}
