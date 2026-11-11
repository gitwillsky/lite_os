//! 冻结选路集的递归下一跳展开；候选过滤、设备事实及本结果必须来自同一管理事务。
//! 成员依赖采用显式 DFS 栈，不把来源控制的链长变成 Rust 调用栈深度。
use super::*;
use core::{net::IpAddr, num::NonZeroU32};
use fallible_tree::FallibleMap;
use router_protocol::routing::Target;
mod forwarding;
pub use forwarding::{Forwarding, SourceExport};
mod address;
pub use address::RibAddress;
mod installation;
mod selection;
mod tracking;
mod weights;
use weights::{Part, Weight};

pub use router_protocol::tracking::{Adjacency, Member};
/// 不包含递归成员的共享结果；相同结果在同一快照内只存一份。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NextHop {
    action: Action,
    members: Vec<Member>,
}
impl NextHop {
    /// 本机交付、转发或策略终止动作；只有 Forward 有递归出口。
    pub fn action(&self) -> Action {
        self.action
    }
    /// 已解析的全部成员；终止动作没有成员。
    pub fn members(&self) -> &[Member] {
        &self.members
    }
}
/// 解析中被剔除的分支；有其他可用成员时仍可以得到非空转发组。
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Issues {
    /// 至少一条分支回到了当前依赖路径，不能作为可达证明。
    pub cycle: bool,
    /// 缺少路由、出口不可用或依赖终止动作。
    pub unreachable: bool,
}
impl Issues {
    fn include(&mut self, other: Self) {
        self.cycle |= other.cycle;
        self.unreachable |= other.unreachable;
    }
}
#[derive(Clone, Copy, Default)]
struct Resolution {
    hop: Option<NonZeroU32>,
    issues: Issues,
}
#[derive(Clone, Copy)]
enum State {
    Unseen,
    Active,
    Cached(Resolution),
}
struct Frame {
    node: usize,
    cursor: usize,
    parts: Vec<Part>,
    issues: Issues,
}
impl Frame {
    fn new(node: usize) -> Self {
        Self {
            node,
            cursor: 0,
            parts: Vec::new(),
            issues: Issues::default(),
        }
    }
}
/// 对同一冻结选路集的借用投影；None 表示不可解析，不能发布为转发路由。
pub struct Selected<'a, A, D, M = ()> {
    /// 实际来源与前缀属性。
    pub candidate: Candidate<'a, A, D, super::NextHop, M>,
    /// 已解析立即出口或终止动作；失败不伪造空转发组。
    pub next_hop: Option<&'a NextHop>,
    /// 被剔除分支的原因。
    pub issues: Issues,
}
/// 一个冻结目录、选定行和共享解析结果构成的不可变控制面快照。
/// 这不是 FIB；失败候选的重选必须重新冻结整份选路集，不能只修改一条已解析结果。
pub struct Resolved<A, D, M = ()> {
    catalogue: Arc<Catalogue<A, D, super::NextHop, M>>,
    selected: Vec<u32>,
    // 同前缀备用按目录顺序下发。它们不参与本代的递归目标、协议宣告或主选诊断。
    backups: Vec<u32>,
    offsets: Vec<usize>,
    results: Vec<Option<Resolution>>,
    groups: Vec<Arc<NextHop>>,
}
impl<A: Address, D, M> Resolved<A, D, M> {
    fn view(&self, row: u32) -> Selected<'_, A, D, M> {
        let reference = self.catalogue.references[row as usize];
        let candidate = self.catalogue.project(reference);
        let node = self.offsets[reference.source as usize] + candidate.route.next_hop as usize;
        let result = self.results[node].expect("selected declaration was resolved");
        Selected {
            candidate,
            next_hop: result
                .hop
                .map(|id| self.groups[id.get() as usize - 1].as_ref()),
            issues: result.issues,
        }
    }
    /// 按前缀顺序借用选定候选，包括不可解析项，供重选/诊断使用；不分配或克隆 Arc。
    pub fn selected(&self) -> impl Iterator<Item = Selected<'_, A, D, M>> + Clone {
        self.selected
            .iter()
            .filter(|row| **row != u32::MAX)
            .map(|row| self.view(*row))
    }
    /// 借用最长已选择前缀；不可解析/终止动作不绕过到父前缀。
    /// # Parameters
    /// `address` 是同一地址族和路由实例中的目的地址。
    /// # Returns
    /// 无选择时 None；存在选择但解析失败时 Selected::next_hop 为 None。
    pub fn lookup(&self, address: A) -> Option<Selected<'_, A, D, M>> {
        self.catalogue
            .index
            .matching_indices(address)
            .find_map(|index| {
                let row = self.selected[index];
                (row != u32::MAX).then(|| self.view(row))
            })
    }
    /// 不同解析组数量；包含有引用的终止动作，不表示 FIB 安装数量。
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }
}
impl<A: RibAddress, D, M> Catalogue<A, D, super::NextHop, M> {
    /// 冻结每个前缀的首个 eligible 候选，并在同一集合中递归展开全部已选择下一跳。
    /// # Parameters
    /// `eligible` 根据冻结策略/来源事实筛选候选；`usable` 基于同一冻结接口事实验证立即出口。
    /// 两个闭包不得在求解期间改变其事实或执行设备 I/O。
    /// # Returns
    /// 保活原目录的控制面快照；不可解析项保留用于后续重选，不直接下发。
    /// # Errors
    /// 分配失败、成员/地址族错误，或精确权重超出可表达范围；不发表部分结果。
    pub fn resolve(
        self: &Arc<Self>,
        mut eligible: impl FnMut(&Candidate<'_, A, D, super::NextHop, M>) -> bool,
        usable: impl FnMut(&Adjacency) -> bool,
    ) -> Result<Resolved<A, D, M>, Error> {
        compile(
            self,
            |row| eligible(&self.project(self.references[row])),
            usable,
            None,
            |_| false,
        )
    }
    /// 按既定优先级重选可解析候选，撤回失效项并恢复可以安全晋升的备用声明。
    /// # Parameters
    /// `eligible`/`usable` 必须读取同一冻结策略与接口事实，整个求解期间不得改变事实或执行 I/O。
    /// `usable` 仅判断立即接口/邻接事实，不得依赖本次求解中变化的候选选择。
    /// # Returns
    /// 只含可解析选择的不可变快照；未选中的声明仍保留在原 Catalogue，新事实到来时重新求解。
    /// # Errors
    /// 内存/表示范围不足或声明/冻结事实不一致；不发表部分选择，不改变原目录。
    pub fn select_resolved(
        self: &Arc<Self>,
        eligible: impl Fn(&Candidate<'_, A, D, super::NextHop, M>) -> bool,
        usable: impl Fn(&Adjacency) -> bool,
    ) -> Result<Resolved<A, D, M>, Error> {
        self.select_with_backups(eligible, usable, |_| false, None)
    }

    /// 同时保留指定来源的可解析备用候选，执行端可根据即时邻接状态切换。
    /// `retain` 只在冷编译中选择可下发的备用声明，不改变首选、递归图或来源资格。
    pub(crate) fn select_with_backups(
        self: &Arc<Self>,
        eligible: impl Fn(&Candidate<'_, A, D, super::NextHop, M>) -> bool,
        usable: impl Fn(&Adjacency) -> bool,
        retain: impl Fn(&Candidate<'_, A, D, super::NextHop, M>) -> bool,
        underlay: Option<&Resolved<u128, D, M>>,
    ) -> Result<Resolved<A, D, M>, Error> {
        selection::select(self, eligible, usable, retain, underlay)
    }
}
struct Compiler<'a, A, D, M> {
    catalogue: &'a Catalogue<A, D, super::NextHop, M>,
    selected: Vec<u32>,
    offsets: Vec<usize>,
    nodes: Vec<&'a super::NextHop>,
    states: Vec<State>,
    groups: Vec<Arc<NextHop>>,
    // OWNER: 冷编译去重索引只保活每个组一次，完成即退休；无逐前缀引用计数。
    by_group: FallibleMap<Arc<NextHop>, NonZeroU32>,
}
fn prepare<A: Address, D, M>(
    catalogue: &Catalogue<A, D, super::NextHop, M>,
    mut eligible: impl FnMut(usize) -> bool,
) -> Result<Compiler<'_, A, D, M>, Error> {
    let mut selected = Vec::new();
    selected
        .try_reserve_exact(catalogue.index.prefixes().len())
        .map_err(|_| Error::NoMemory)?;
    for prefix in catalogue.index.prefixes() {
        selected.push(
            prefix
                .range()
                .find(|row| eligible(*row))
                .map_or(u32::MAX, |row| row as u32),
        );
    }
    let mut offsets = Vec::new();
    offsets
        .try_reserve_exact(catalogue.sources.len())
        .map_err(|_| Error::NoMemory)?;
    let mut count = 0usize;
    for source in &catalogue.sources {
        offsets.push(count);
        count = count
            .checked_add(source.definition.next_hops.len())
            .ok_or(Error::Capacity)?;
    }
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    nodes.extend(
        catalogue
            .sources
            .iter()
            .flat_map(|source| source.definition.next_hops.iter()),
    );
    let mut states = Vec::new();
    states
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    states.resize(count, State::Unseen);
    Ok(Compiler {
        catalogue,
        selected,
        offsets,
        nodes,
        states,
        groups: Vec::new(),
        by_group: FallibleMap::new(),
    })
}
fn compile<A: RibAddress, D, M>(
    catalogue: &Arc<Catalogue<A, D, super::NextHop, M>>,
    eligible: impl FnMut(usize) -> bool,
    mut usable: impl FnMut(&Adjacency) -> bool,
    underlay: Option<&Resolved<u128, D, M>>,
    retain: impl Fn(usize) -> bool,
) -> Result<Resolved<A, D, M>, Error> {
    let mut compiler = prepare(catalogue, eligible)?;
    let mut results = Vec::new();
    results
        .try_reserve_exact(compiler.nodes.len())
        .map_err(|_| Error::NoMemory)?;
    results.resize(compiler.nodes.len(), None);
    let mut work = Vec::new();
    for index in 0..compiler.selected.len() {
        let row = compiler.selected[index];
        if row == u32::MAX {
            continue;
        }
        let node = compiler.node(row);
        if results[node].is_none() {
            results[node] = Some(compiler.walk(node, &mut usable, underlay, &mut work)?);
        }
    }
    let mut backups = Vec::new();
    // 仅在主选图稳定时解析较低优先级声明。不能让备用的不可解析结果参加主选重算。
    if results.iter().flatten().all(|result| result.hop.is_some()) {
        for (index, prefix) in catalogue.index.prefixes().iter().enumerate() {
            let row = compiler.selected[index];
            if row == u32::MAX {
                continue;
            }
            let mut candidates = ((row as usize + 1)..prefix.range().end)
                .filter(|row| retain(*row))
                .peekable();
            if candidates.peek().is_none() {
                continue;
            }
            let primary = results[compiler.node(row)].expect("resolved primary");
            let primary = primary.hop.expect("stable primary graph");
            if matches!(
                compiler.groups[primary.get() as usize - 1].action,
                Action::Discard | Action::Unreachable | Action::Prohibit
            ) {
                continue;
            }
            for candidate in candidates {
                let node = compiler.node(candidate as u32);
                let result = match results[node] {
                    Some(result) => result,
                    None => compiler.walk(node, &mut usable, underlay, &mut work)?,
                };
                let Some(hop) = result.hop else {
                    continue;
                };
                if hop == primary {
                    continue;
                }
                backups.try_reserve(1).map_err(|_| Error::NoMemory)?;
                backups.push(candidate as u32);
                results[node] = Some(result);
                if compiler.groups[hop.get() as usize - 1].action != Action::Forward {
                    break;
                }
            }
        }
    }
    // 只有最终根结果可以进入快照；上下文剪枝产生的中间组在编译结束时退休。
    drop(compiler.by_group);
    let mut identities = Vec::new();
    identities
        .try_reserve_exact(compiler.groups.len())
        .map_err(|_| Error::NoMemory)?;
    identities.resize(compiler.groups.len(), None);
    for result in results.iter().flatten() {
        if let Some(id) = result.hop {
            identities[id.get() as usize - 1] = Some(id);
        }
    }
    let mut groups = Vec::new();
    groups
        .try_reserve_exact(identities.iter().flatten().count())
        .map_err(|_| Error::NoMemory)?;
    for (index, hop) in compiler.groups.into_iter().enumerate() {
        if identities[index].is_some() {
            groups.push(hop);
            identities[index] = NonZeroU32::new(groups.len() as u32);
        }
    }
    for result in results.iter_mut().flatten() {
        result.hop = result.hop.and_then(|id| identities[id.get() as usize - 1]);
    }
    Ok(Resolved {
        catalogue: catalogue.clone(),
        selected: compiler.selected,
        backups,
        offsets: compiler.offsets,
        results,
        groups,
    })
}
impl<A: RibAddress, D, M> Compiler<'_, A, D, M> {
    fn node(&self, row: u32) -> usize {
        let reference = self.catalogue.references[row as usize];
        self.offsets[reference.source as usize]
            + self.catalogue.project(reference).route.next_hop as usize
    }
    fn target(&self, address: A) -> Option<usize> {
        self.catalogue
            .index
            .matching_indices(address)
            .find_map(|index| {
                let row = self.selected[index];
                (row != u32::MAX).then(|| self.node(row))
            })
    }
    fn intern(&mut self, hop: NextHop) -> Result<NonZeroU32, Error> {
        if let Some(id) = self.by_group.get(&hop) {
            return Ok(*id);
        }
        let id = u32::try_from(self.groups.len())
            .ok()
            .and_then(|value| value.checked_add(1))
            .and_then(NonZeroU32::new)
            .ok_or(Error::Capacity)?;
        self.groups.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let hop = Arc::try_new(hop).map_err(|_| Error::NoMemory)?;
        let entry = self
            .by_group
            .try_prepare_vacant(hop.clone(), id)
            .map_err(|_| Error::NoMemory)?;
        self.by_group.commit_vacant(entry);
        self.groups.push(hop);
        Ok(id)
    }
    fn append_recursive(
        frame: &mut Frame,
        member: router_protocol::routing::Member,
        child: Option<&NextHop>,
        usable: &mut impl FnMut(&Adjacency) -> bool,
    ) -> Result<(), Error> {
        let Target::Recursive(address) = member.target else {
            unreachable!("only recursive members await a child");
        };
        if let Some(hop) = child.filter(|hop| hop.action == Action::Forward) {
            let start = frame.parts.len();
            let mut total = 0u64;
            frame
                .parts
                .try_reserve(hop.members.len())
                .map_err(|_| Error::NoMemory)?;
            for resolved in &hop.members {
                let mut target = resolved.target;
                // 直接相连前缀没有网关时，使用本层 NEXT_HOP；已有立即网关则保持它。
                target.gateway = target.gateway.or(Some(address));
                if usable(&target) {
                    total += u64::from(resolved.weight);
                    frame.parts.push(Part {
                        target,
                        weight: Weight::direct(resolved.weight),
                    });
                } else {
                    frame.issues.unreachable = true;
                }
            }
            // 新立即网关可能使部分出口失效；按本层仍可用成员重新分配父路径权重。
            // 空区间不执行除法，所有存入 Part 的成员均有正权重，所以非空区间 total > 0。
            for part in &mut frame.parts[start..] {
                part.weight = part.weight.scale(member.weight, total)?;
            }
        } else {
            frame.issues.unreachable = true;
        }
        frame.cursor += 1;
        Ok(())
    }
    fn walk(
        &mut self,
        root: usize,
        usable: &mut impl FnMut(&Adjacency) -> bool,
        underlay: Option<&Resolved<u128, D, M>>,
        stack: &mut Vec<Frame>,
    ) -> Result<Resolution, Error> {
        if let State::Cached(result) = self.states[root] {
            return Ok(result);
        }
        // 顺序编译各根时复用已弹空的工作栈；没有跨快照存储或每根堆栈分配。
        stack.try_reserve(1).map_err(|_| Error::NoMemory)?;
        stack.push(Frame::new(root));
        self.states[root] = State::Active;
        let mut returned = None;
        loop {
            if let Some(result) = returned.take() {
                let Some(frame) = stack.last_mut() else {
                    return Ok(result);
                };
                let member = self.nodes[frame.node].members()[frame.cursor];
                let child = result
                    .hop
                    .map(|id| self.groups[id.get() as usize - 1].as_ref());
                frame.issues.include(result.issues);
                Self::append_recursive(frame, member, child, usable)?;
                continue;
            }
            let frame = stack.last_mut().expect("root frame exists until returned");
            let declaration = self.nodes[frame.node];
            if declaration.action() != Action::Forward
                || frame.cursor == declaration.members().len()
            {
                let members = weights::normalize(core::mem::take(&mut frame.parts))?;
                let hop = if declaration.action() != Action::Forward || !members.is_empty() {
                    Some(self.intern(NextHop {
                        action: declaration.action(),
                        members,
                    })?)
                } else {
                    None
                };
                let result = Resolution {
                    hop,
                    issues: frame.issues,
                };
                // 环路剪枝的结果依赖当前祖先，不能作为另一根的正/负 memo；无环子图可以安全共享。
                self.states[frame.node] = if result.issues.cycle {
                    State::Unseen
                } else {
                    State::Cached(result)
                };
                stack.pop();
                returned = Some(result);
                continue;
            }
            let member = declaration.members()[frame.cursor];
            match member.target {
                Target::Direct {
                    interface_index,
                    gateway,
                    on_link,
                } => {
                    let target = Adjacency {
                        interface_index,
                        gateway,
                        on_link,
                    };
                    if usable(&target) {
                        frame.parts.try_reserve(1).map_err(|_| Error::NoMemory)?;
                        frame.parts.push(Part {
                            target,
                            weight: Weight::direct(member.weight),
                        });
                    } else {
                        frame.issues.unreachable = true;
                    }
                    frame.cursor += 1;
                }
                Target::Recursive(address) => {
                    let Some(index) = A::from_ip(address) else {
                        // 只有 IPv4 依赖已冻结的 IPv6 图；跨族边不会进入本族 DFS 栈或回到 IPv4。
                        let child =
                            underlay.and_then(|underlay| lookup_underlay(underlay, address));
                        if let Some(child) = &child {
                            frame.issues.include(child.issues);
                        }
                        Self::append_recursive(
                            frame,
                            member,
                            child.and_then(|child| child.next_hop),
                            usable,
                        )?;
                        continue;
                    };
                    match self.target(index).map(|node| (node, self.states[node])) {
                        None => {
                            frame.issues.unreachable = true;
                            frame.cursor += 1;
                        }
                        Some((_, State::Active)) => {
                            frame.issues.cycle = true;
                            frame.cursor += 1;
                        }
                        Some((_, State::Cached(result))) => returned = Some(result),
                        Some((node, State::Unseen)) => {
                            stack.try_reserve(1).map_err(|_| Error::NoMemory)?;
                            self.states[node] = State::Active;
                            stack.push(Frame::new(node));
                        }
                    }
                }
            }
        }
    }
}

fn lookup_underlay<D, M>(
    underlay: &Resolved<u128, D, M>,
    address: IpAddr,
) -> Option<Selected<'_, u128, D, M>> {
    let IpAddr::V6(address) = address else {
        return None;
    };
    underlay.lookup(u128::from(address))
}

#[cfg(test)]
mod tests;
