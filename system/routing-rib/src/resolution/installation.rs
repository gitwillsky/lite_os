//! 在同一冻结解析图上检查安装后自递归；只搜索仍有效的出口，不构建另一份 FIB 或修改选定行。
use super::*;
use router_protocol::tracking::installation::{Installation, Outcome};

struct Search<'a, A: RibAddress, D, M> {
    resolved: &'a Resolved<A, D, M>,
    underlay: Option<&'a Resolved<u128, D, M>>,
    // OWNER: 只属于本批搜索的 visited 位图；每个检查清除实际触及的字，不缓存跨前缀/代际的准入结果。
    seen: Vec<u64>,
    touched: Vec<usize>,
    work: Vec<Reference>,
}
impl<'a, A: RibAddress, D, M> Search<'a, A, D, M>
where
    A::Ip: Into<IpAddr>,
{
    fn new(
        resolved: &'a Resolved<A, D, M>,
        underlay: Option<&'a Resolved<u128, D, M>>,
    ) -> Result<Self, Error> {
        let mut seen = Vec::new();
        seen.try_reserve_exact(resolved.results.len().div_ceil(64))
            .map_err(|_| Error::NoMemory)?;
        seen.resize(resolved.results.len().div_ceil(64), 0);
        Ok(Self {
            resolved,
            underlay,
            seen,
            touched: Vec::new(),
            work: Vec::new(),
        })
    }
    fn reset(&mut self) {
        for word in self.touched.drain(..) {
            self.seen[word] = 0;
        }
        self.work.clear();
    }
    fn lookup(&self, address: IpAddr, installation: Installation) -> Option<Reference> {
        let resolved = self.resolved;
        resolved
            .catalogue
            .index
            .matching_indices(A::from_ip(address)?)
            .find_map(|index| {
                let row = resolved.selected[index];
                (row != u32::MAX).then(|| resolved.catalogue.references[row as usize])
            })
            .filter(|reference| {
                let route = &resolved.catalogue.sources[reference.source as usize].routes
                    [reference.route as usize];
                // 新前缀会接管这一 LPM，便回到待检查路由自身；这是失败分支，不能再退到祖先或旧出口。
                !(installation.contains(address) && installation.length >= route.length)
            })
    }
    fn push(
        &mut self,
        reference: Reference,
        address: IpAddr,
        usable: &impl Fn(&Adjacency) -> bool,
    ) -> Result<bool, Error> {
        let source = reference.source as usize;
        let definition = &self.resolved.catalogue.sources[source];
        let route = &definition.routes[reference.route as usize];
        let node = self.resolved.offsets[source] + route.next_hop as usize;
        let declaration = &definition.definition.next_hops[route.next_hop as usize];
        if declaration.action() != Action::Forward {
            return Ok(false);
        }
        // 先按本次入边检查直接出口，再去重递归展开。同一字典由不同地址进入时，立即网关可能不同。
        // NextHop 捕获时按 Target 排序，Direct 连续在前；不要为每条入边重新扫描整段递归成员。
        for member in declaration
            .members()
            .iter()
            .take_while(|member| matches!(member.target, Target::Direct { .. }))
        {
            if let Target::Direct {
                interface_index,
                gateway,
                on_link,
            } = member.target
                && usable(&Adjacency {
                    interface_index,
                    gateway: gateway.or(Some(address)),
                    on_link,
                })
            {
                return Ok(true);
            }
        }
        let word = node / 64;
        let bit = 1 << (node % 64);
        if self.seen[word] & bit != 0 {
            return Ok(false);
        }
        self.work.try_reserve(1).map_err(|_| Error::NoMemory)?;
        if self.seen[word] == 0 {
            self.touched.try_reserve(1).map_err(|_| Error::NoMemory)?;
            self.touched.push(word);
        }
        self.seen[word] |= bit;
        self.work.push(reference);
        Ok(false)
    }
    fn reachable(
        &mut self,
        installation: Installation,
        usable: &impl Fn(&Adjacency) -> bool,
    ) -> Result<bool, Error> {
        self.reset();
        let Some(root) = self.lookup(installation.next_hop.address, installation) else {
            return Ok(false);
        };
        let root_route =
            &self.resolved.catalogue.sources[root.source as usize].routes[root.route as usize];
        if root_route.length == 0 && !installation.next_hop.resolve_via_default {
            return Ok(false);
        }
        if self.push(root, installation.next_hop.address, usable)? {
            return Ok(true);
        }
        let resolved = self.resolved;
        while let Some(reference) = self.work.pop() {
            let definition = &resolved.catalogue.sources[reference.source as usize];
            let route = &definition.routes[reference.route as usize];
            for member in definition.definition.next_hops[route.next_hop as usize].members() {
                match member.target {
                    Target::Recursive(address) => {
                        if A::from_ip(address).is_none() {
                            // 本候选只能改变 IPv4 LPM，已冻结的 IPv6 依赖不受反事实覆盖影响。
                            if self
                                .underlay
                                .and_then(|graph| lookup_underlay(graph, address))
                                .and_then(|selected| selected.next_hop)
                                .is_some_and(|hop| {
                                    hop.action() == Action::Forward
                                        && hop.members().iter().any(|member| {
                                            let mut target = member.target;
                                            target.gateway = target.gateway.or(Some(address));
                                            usable(&target)
                                        })
                                })
                            {
                                return Ok(true);
                            }
                            continue;
                        }
                        if let Some(child) = self.lookup(address, installation)
                            && self.push(child, address, usable)?
                        {
                            return Ok(true);
                        }
                    }
                    Target::Direct { .. } => {}
                }
            }
        }
        Ok(false)
    }
}
impl<A: RibAddress, D, M> Resolved<A, D, M>
where
    A::Ip: Into<IpAddr>,
{
    /// 批量检查每个候选在本图上覆盖目的前缀后，是否仍有不经自身的有效转发路径。
    /// # Parameters
    /// `probes` 是规范候选目的与逻辑下一跳；更具体既有前缀仍优先，终止动作不退向祖先。
    /// `usable` 必须使用与编译本图相同的冻结接口/邻接事实，包含递归展开后的立即网关；不得查旧 FIB。
    /// `interior_cost` 只能由可信提供者返回真实 IGP 成本，未知必须 None；它不参与可达性推断。
    /// # Returns
    /// 与输入等长/同序的判断，不修改图；它们只对这份冻结图有效，不能替代整批候选及执行确认。
    /// # Errors
    /// 非规范输入、下一跳不在本图地址族或工作区/输出分配失败；不返回部分结果。
    pub fn evaluate_installations(
        &self,
        probes: &[Installation],
        usable: impl Fn(&Adjacency) -> bool,
        interior_cost: impl Fn(&Candidate<'_, A, D, super::super::NextHop, M>) -> Option<u32>,
    ) -> Result<Vec<Outcome>, Error> {
        if probes
            .iter()
            .any(|probe| A::from_ip(probe.next_hop.address).is_none())
        {
            return Err(Error::InvalidPrefix);
        }
        let mut outcomes = Vec::new();
        outcomes
            .try_reserve_exact(probes.len())
            .map_err(|_| Error::NoMemory)?;
        outcomes.resize(
            probes.len(),
            Outcome {
                reachable: false,
                interior_cost: None,
            },
        );
        self.evaluate_installations_into(probes, &mut outcomes, usable, interior_cost, None)?;
        Ok(outcomes)
    }
    /// 只填充本族对应位置；两族共享同一最终结果区，不复制检查数组或创建额外结果向量。
    /// # Errors
    /// 数量不匹配、检查非法或搜索工作区分配失败；调用者在整批成功前不得发表结果。
    pub(crate) fn evaluate_installations_into(
        &self,
        probes: &[Installation],
        outcomes: &mut [Outcome],
        usable: impl Fn(&Adjacency) -> bool,
        interior_cost: impl Fn(&Candidate<'_, A, D, super::super::NextHop, M>) -> Option<u32>,
        underlay: Option<&Resolved<u128, D, M>>,
    ) -> Result<(), Error> {
        if probes.len() != outcomes.len() || probes.iter().any(|probe| !probe.valid()) {
            return Err(Error::InvalidPrefix);
        }
        let mut search = None;
        for (probe, outcome) in probes.iter().zip(outcomes) {
            let Some(address) = A::from_ip(probe.next_hop.address) else {
                continue;
            };
            if let Some(interface) = probe.interface {
                let reachable = usable(&Adjacency {
                    interface_index: interface.get(),
                    gateway: Some(probe.next_hop.address),
                    on_link: true,
                });
                *outcome = Outcome {
                    reachable,
                    interior_cost: reachable.then_some(0),
                };
                continue;
            }
            let search = match &mut search {
                Some(search) => search,
                empty => empty.insert(Search::new(self, underlay)?),
            };
            let reachable = search.reachable(*probe, &usable)?;
            let interior_cost = if reachable {
                self.lookup(address)
                    .and_then(|selected| interior_cost(&selected.candidate))
            } else {
                None
            };
            *outcome = Outcome {
                reachable,
                interior_cost,
            };
        }
        Ok(())
    }
}
