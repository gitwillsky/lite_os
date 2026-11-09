//! 候选准入与依赖恢复。拒绝掩码只属于一次冻结求解，不是跨代际可达性缓存。
use super::*;

struct Marks {
    excluded: Vec<u64>,
    sensitive: Vec<u64>,
}
impl Marks {
    fn words(count: usize) -> Result<Vec<u64>, Error> {
        let mut words = Vec::new();
        words
            .try_reserve_exact(count.div_ceil(64))
            .map_err(|_| Error::NoMemory)?;
        words.resize(count.div_ceil(64), 0);
        Ok(words)
    }
    fn new<A: Address, D, M>(
        catalogue: &Catalogue<A, D, super::super::NextHop, M>,
    ) -> Result<Self, Error> {
        Ok(Self {
            excluded: Self::words(catalogue.references.len())?,
            sensitive: Self::words(catalogue.index.prefixes().len())?,
        })
    }
    fn capture_dependencies<A: Address, D, M>(
        &mut self,
        stable: &Resolved<A, D, M>,
        convert: fn(IpAddr) -> Option<A>,
    ) -> Result<(), Error> {
        let catalogue = &stable.catalogue;
        // 任何候选所引用的递归地址都参与依赖判定；不能只看当前已选来源，否则晋升会漏掉新依赖。
        for source in &catalogue.sources {
            for hop in source.definition.next_hops.iter() {
                for member in hop.members() {
                    if let Target::Recursive(address) = member.target {
                        let address = convert(address).ok_or(Error::InvalidSource)?;
                        for index in catalogue.index.matching_indices(address) {
                            self.sensitive[index / 64] |= 1 << (index % 64);
                            // 恢复阶段只增加覆盖或升级候选，不撤回已有有效前缀；被它遮住的祖先不能改变此地址的 LPM。
                            if stable.selected[index] != u32::MAX {
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn excluded(&self, row: usize) -> bool {
        self.excluded[row / 64] & (1 << (row % 64)) != 0
    }
    fn exclude(&mut self, row: usize) {
        self.excluded[row / 64] |= 1 << (row % 64);
    }
    fn restore(&mut self, row: usize) {
        self.excluded[row / 64] &= !(1 << (row % 64));
    }
    fn sensitive(&self, prefix: usize) -> bool {
        self.sensitive[prefix / 64] & (1 << (prefix % 64)) != 0
    }
    fn reject_failed<A: Address, D, M>(&mut self, result: &Resolved<A, D, M>) {
        // 同一失败字典对象的别名一起暂退，避免百万同对象候选按行逐次重新编译；声明不删除，随后仍可恢复。
        for (row, reference) in result.catalogue.references.iter().enumerate() {
            let route = &result.catalogue.sources[reference.source as usize].routes
                [reference.route as usize];
            let node = result.offsets[reference.source as usize] + route.next_hop as usize;
            if result.results[node].is_some_and(|result| result.hop.is_none()) {
                self.exclude(row);
            }
        }
    }
}
fn unresolved<A, D, M>(result: &Resolved<A, D, M>) -> bool {
    result
        .results
        .iter()
        .flatten()
        .any(|result| result.hop.is_none())
}
struct Availability {
    offsets: Vec<usize>,
    ready: Vec<bool>,
}
impl Availability {
    fn node<A: Address, D, M>(
        &self,
        catalogue: &Catalogue<A, D, super::super::NextHop, M>,
        row: usize,
    ) -> usize {
        let reference = catalogue.references[row];
        self.offsets[reference.source as usize]
            + catalogue.sources[reference.source as usize].routes[reference.route as usize].next_hop
                as usize
    }
    fn capture<A: Address, D, M>(
        catalogue: &Catalogue<A, D, super::super::NextHop, M>,
        marks: &Marks,
        eligible: &impl Fn(&Candidate<'_, A, D, super::super::NextHop, M>) -> bool,
        usable: &impl Fn(&Adjacency) -> bool,
        convert: fn(IpAddr) -> Option<A>,
    ) -> Result<Self, Error> {
        let mut compiler = prepare(catalogue, |row| {
            !marks.excluded(row) && eligible(&catalogue.project(catalogue.references[row]))
        })?;
        let mut ready = Vec::new();
        ready
            .try_reserve_exact(compiler.nodes.len())
            .map_err(|_| Error::NoMemory)?;
        let mut work = Vec::new();
        let mut usable = |target: &Adjacency| usable(target);
        for node in 0..compiler.nodes.len() {
            ready.push(
                compiler
                    .walk(node, &mut usable, convert, &mut work)?
                    .hop
                    .is_some(),
            );
        }
        Ok(Self {
            offsets: compiler.offsets,
            ready,
        })
    }
}

pub(super) fn select<A: Address, D, M>(
    catalogue: &Arc<Catalogue<A, D, super::super::NextHop, M>>,
    eligible: impl Fn(&Candidate<'_, A, D, super::super::NextHop, M>) -> bool,
    usable: impl Fn(&Adjacency) -> bool,
    convert: fn(IpAddr) -> Option<A>,
) -> Result<Resolved<A, D, M>, Error> {
    let mut result = compile(
        catalogue,
        |row| eligible(&catalogue.project(catalogue.references[row])),
        &usable,
        convert,
    )?;
    if !unresolved(&result) {
        return Ok(result);
    }
    let mut marks = Marks::new(catalogue)?;
    // 1. 每轮至少暂退一个当前失败选择；掩码只增加，至多覆盖全部声明，所以必然得到稳定的可解析子集。
    while unresolved(&result) {
        marks.reject_failed(&result);
        result = compile(
            catalogue,
            |row| !marks.excluded(row) && eligible(&catalogue.project(catalogue.references[row])),
            &usable,
            convert,
        )?;
    }
    marks.capture_dependencies(&result, convert)?;
    let sensitive = marks.sensitive.iter().any(|word| *word != 0);
    let mut tried = Vec::new();
    if sensitive {
        tried
            .try_reserve_exact(result.results.len())
            .map_err(|_| Error::NoMemory)?;
        tried.resize(result.results.len(), 0u32);
    }
    loop {
        // 2. 不改变任何递归目标 LPM 的前缀不会改变依赖图：按字典解析一次，批量恢复各前缀最优的可用声明。
        let available = Availability::capture(catalogue, &marks, &eligible, &usable, convert)?;
        let mut changed = false;
        for (index, prefix) in catalogue.index.prefixes().iter().enumerate() {
            if marks.sensitive(index) {
                continue;
            }
            for row in prefix.range() {
                if eligible(&catalogue.project(catalogue.references[row]))
                    && available.ready[available.node(catalogue, row)]
                {
                    if marks.excluded(row) {
                        marks.restore(row);
                        changed = true;
                    }
                    break;
                }
            }
        }
        if changed {
            result = compile(
                catalogue,
                |row| {
                    !marks.excluded(row) && eligible(&catalogue.project(catalogue.references[row]))
                },
                &usable,
                convert,
            )?;
            // 只可能由非冻结事实/错误的依赖分类触发；拒绝整次编译，不发布不完整结果。
            if unresolved(&result) {
                return Err(Error::InvalidSource);
            }
        }
        if !sensitive {
            return Ok(result);
        }
        // 3. 会改变依赖图的晋升每次只改一个前缀，完整复核全部已有选择；同前缀/同对象的失败试探只做一次。
        // tried 每轮清空、每前缀换身份，不跨事实或其他前缀晋升保留失败结论。
        tried.fill(0);
        // 非敏感晋升不改变图；只在真正改变依赖的晋升后重算下一轮，否则会多遍历一次全表。
        let mut graph_changed = false;
        for (index, prefix) in catalogue.index.prefixes().iter().enumerate() {
            if !marks.sensitive(index) {
                continue;
            }
            let current = result.selected[index];
            for row in prefix
                .range()
                .take_while(|row| current == u32::MAX || *row < current as usize)
            {
                if !marks.excluded(row) || !eligible(&catalogue.project(catalogue.references[row]))
                {
                    continue;
                }
                let node = available.node(catalogue, row);
                let identity = index as u32 + 1;
                if tried[node] == identity {
                    continue;
                }
                tried[node] = identity;
                marks.restore(row);
                let trial = compile(
                    catalogue,
                    |row| {
                        !marks.excluded(row)
                            && eligible(&catalogue.project(catalogue.references[row]))
                    },
                    &usable,
                    convert,
                )?;
                if unresolved(&trial) {
                    marks.exclude(row);
                } else {
                    result = trial;
                    graph_changed = true;
                    break;
                }
            }
        }
        // 成功晋升严格提高某个前缀的既定优先级，其余前缀选择不变；掩码恢复至多声明数次，不会振荡。
        if !graph_changed {
            return Ok(result);
        }
    }
}
