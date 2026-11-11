//! 为备选表复用同一个静态来源、递归解析及 FIB 投影。

use super::*;
use crate::resolution::{Adjacency, Forwarding, Resolved, RibAddress};
use router_protocol::policy_routing::Rules;

type DeclarationFor<A> = Arc<Source<A, (), NextHop, Origin>>;

struct StaticTable {
    id: u32,
    four: DeclarationFor<u32>,
    six: DeclarationFor<u128>,
}

pub(super) struct StaticTables {
    tables: Vec<StaticTable>,
    rules: Vec<u8>,
}

impl StaticTables {
    pub(super) fn capture(batch: Tables<'_>) -> Result<Arc<Self>, Status> {
        let declarations = batch.iter();
        let mut tables = Vec::new();
        tables
            .try_reserve_exact(declarations.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for (id, [four, six]) in declarations {
            // 来源 ID 仅在各表的独立目录内有意义；来源协议和优先级沿用主表的静态来源约定。
            let capture =
                |body| Routes::capture_source(2, CONFIGURATION_DOMAIN, STATIC_PROTOCOL, 1, body);
            let Declaration::Ipv4(four) = capture(four)?.declaration else {
                return Err(native_abi::ERR_INVALID_ARGS);
            };
            let Declaration::Ipv6(six) = capture(six)?.declaration else {
                return Err(native_abi::ERR_INVALID_ARGS);
            };
            tables.push(StaticTable { id, four, six });
        }
        for rule in batch.rules().iter() {
            if tables
                .binary_search_by_key(&rule.table, |table| table.id)
                .is_err()
            {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
        }
        let encoded = batch.rules().bytes();
        let mut rules = Vec::new();
        rules
            .try_reserve_exact(encoded.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        rules.extend_from_slice(encoded);
        Arc::try_new(Self { tables, rules }).map_err(|_| native_abi::ERR_NO_MEMORY)
    }

    pub(super) fn rules(&self) -> Rules<'_> {
        Rules::parse(&self.rules).expect("captured selection rules")
    }
}

pub(super) struct SolvedTable {
    pub(super) id: u32,
    pub(super) four: Resolved<u32, (), Origin>,
    pub(super) six: Resolved<u128, (), Origin>,
}

pub(super) fn solve(
    tables: Option<&StaticTables>,
    observed4: DeclarationFor<u32>,
    observed6: DeclarationFor<u128>,
    lookup: Lookup<'_>,
    usable4: impl Fn(&Adjacency) -> bool,
    usable6: impl Fn(&Adjacency) -> bool,
) -> Result<Vec<SolvedTable>, Status> {
    let mut result = Vec::new();
    let Some(tables) = tables else {
        return Ok(result);
    };
    result
        .try_reserve_exact(tables.tables.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for table in &tables.tables {
        let six = super::solve(
            core::iter::once(&table.six),
            observed6.clone(),
            lookup,
            &usable6,
            |_| true,
            None,
        )?;
        let four = super::solve(
            core::iter::once(&table.four),
            observed4.clone(),
            lookup,
            &usable4,
            |_| false,
            Some(&six),
        )?;
        result.push(SolvedTable {
            id: table.id,
            four,
            six,
        });
    }
    Ok(result)
}

pub(super) fn projection<'a, A: RibAddress>(
    main: &'a Resolved<A, (), Origin>,
    alternate: impl ExactSizeIterator<Item = (u32, &'a Resolved<A, (), Origin>)>,
) -> Result<Vec<Forwarding<'a, A, (), Origin>>, Status> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(1 + alternate.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut cursor = 0;
    for (table, resolved) in
        core::iter::once((router_protocol::policy_routing::TABLE_MAIN, main)).chain(alternate)
    {
        let mut forwarding = resolved.forwarding(table, export)?;
        forwarding.assign_ids(&mut cursor)?;
        result.push(forwarding);
    }
    Ok(result)
}

/// 保留分表冻结投影的精确总数，编码时无需再收集全部前缀或组。
pub(super) fn concatenate<'a, T, I, F>(
    tables: &'a [T],
    project: F,
) -> Result<impl Clone + ExactSizeIterator<Item = I::Item> + 'a, Status>
where
    T: 'a,
    F: Clone + Fn(&'a T) -> I + 'a,
    I: Clone + ExactSizeIterator + 'a,
{
    let count = tables.iter().try_fold(0usize, |count, table| {
        count
            .checked_add(project(table).len())
            .ok_or(native_abi::ERR_OUT_OF_RANGE)
    })?;
    let mut rows = tables.iter().flat_map(project);
    Ok((0..count).map(move |_| rows.next().expect("frozen forwarding count")))
}
