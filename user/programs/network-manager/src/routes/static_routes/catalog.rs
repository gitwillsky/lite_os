//! 静态声明按配置代际与表内位置识别。运行时路径拆分、递归解析或 ECMP 合并后不能反查声明。

use liteos_ui::Background;
use network_config::{
    ManagementClient, Revision,
    configuration::{
        IpPrefix, RouterConfiguration, StaticAction, StaticRoute, StaticTable, StaticTarget,
    },
    routes::{
        Family, TABLE_MAIN,
        selection::{self, Candidate, State},
    },
};
use std::{collections::BTreeMap, io, num::NonZeroU32, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Key {
    pub table: u32,
    index: usize,
}

impl Key {
    fn location(self) -> Option<NonZeroU32> {
        u32::try_from(self.index)
            .ok()?
            .checked_add(1)
            .and_then(NonZeroU32::new)
    }
    pub(super) fn candidate(self, route: &StaticRoute) -> Option<Candidate> {
        Some(Candidate {
            prefix: IpPrefix {
                network: route.destination,
                length: route.prefix_length,
            },
            location: self.location()?,
        })
    }
}

pub(super) struct Entry {
    pub key: Key,
    pub route: StaticRoute,
    pub text: String,
    pub status: Option<State>,
}

impl Entry {
    fn show_status(&mut self, status: Option<State>) {
        self.status = status;
        let (flag, distance) = match status {
            Some(State::Active(metrics)) => ("As", metrics.distance.get()),
            Some(State::Standby(metrics)) => ("s", metrics.distance.get()),
            Some(State::Unreachable(metrics)) => ("Is", metrics.distance.get()),
            Some(State::Absent) => ("Is", self.route.preference),
            None => (
                if self.route.disabled { "Xs" } else { "s" },
                self.route.preference,
            ),
        };
        self.text = format!(
            "{flag}\t{}/{}\t{}\t{distance}\t{}",
            self.route.destination,
            self.route.prefix_length,
            gateway_text(&self.route.action),
            table_name(self.key.table),
        );
    }
}

pub(super) type Selections = BTreeMap<(u32, NonZeroU32), State>;

pub(super) struct SelectionRead {
    pub revision: Revision,
    tables: BTreeMap<u32, Vec<Candidate>>,
}

impl SelectionRead {
    /// 每页核对声明与确认图代际，整轮一致才显示候选状态。
    pub(super) async fn read(
        self,
        background: &Background,
        scope: Option<Arc<str>>,
    ) -> Option<Selections> {
        let mut answers = Selections::new();
        let mut graph = None;
        for (table, candidates) in self.tables {
            for chunk in candidates.chunks(selection::LIMIT) {
                let request = chunk.to_vec();
                let domain = scope.clone();
                let (revision, result) = background
                    .run(move || {
                        ManagementClient::local().static_status(domain.as_deref(), table, &request)
                    })
                    .ok()?
                    .await
                    .ok()?;
                if revision != self.revision
                    || graph.is_some_and(|generation| generation != result.generation)
                {
                    return None;
                }
                graph = Some(result.generation);
                for (candidate, state) in chunk.iter().zip(result.rows) {
                    answers.insert((table, candidate.location), state);
                }
            }
        }
        Some(answers)
    }
}

pub(super) struct Catalog {
    pub revision: Revision,
    pub tables: Vec<u32>,
    pub rows: [Vec<Entry>; 2],
}

pub(super) enum Change {
    Put {
        original: Option<Key>,
        table: u32,
        route: StaticRoute,
    },
    Remove(Key),
}

fn read(client: &mut ManagementClient) -> io::Result<(Revision, RouterConfiguration)> {
    let (revision, bytes) = client.export()?;
    let config = RouterConfiguration::decode(&bytes).map_err(io::Error::other)?;
    Ok((revision, config))
}

fn missing_domain() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选路由域已删除。")
}

impl Catalog {
    /// 只在原生配置代际变化后读取完整声明。没有改变时继续使用已有投影。
    pub(super) fn load(
        domain: Option<&str>,
        current: Option<Revision>,
    ) -> io::Result<Option<Self>> {
        let mut client = ManagementClient::local();
        if let Some(current) = current
            && client.snapshot()?.generation == current
        {
            return Ok(None);
        }
        let (revision, config) = read(&mut client)?;
        let (main, policy) = if let Some(domain) = domain {
            let scope = config
                .routing_domains
                .get(domain)
                .ok_or_else(missing_domain)?;
            (&scope.routes, &scope.policy_routing)
        } else {
            (&config.routes, &config.policy_routing)
        };
        Ok(Some(Self::project(revision, main, &policy.tables)))
    }

    fn project(revision: Revision, main: &[StaticRoute], alternate: &[StaticTable]) -> Self {
        let tables = std::iter::once((TABLE_MAIN, main)).chain(
            alternate
                .iter()
                .map(|table| (table.id, table.routes.as_slice())),
        );
        let mut catalog = Self {
            revision,
            tables: Vec::new(),
            rows: [Vec::new(), Vec::new()],
        };
        for (table, routes) in tables {
            catalog.tables.push(table);
            for (index, route) in routes.iter().enumerate() {
                let rows = &mut catalog.rows[usize::from(route.destination.is_ipv6())];
                let mut entry = Entry {
                    key: Key { table, index },
                    route: route.clone(),
                    text: String::new(),
                    status: None,
                };
                entry.show_status(None);
                rows.push(entry);
            }
        }
        for rows in &mut catalog.rows {
            rows.sort_by_key(|entry| (entry.route.destination, entry.route.prefix_length));
        }
        catalog
    }

    pub(super) fn entries(&self, family: Family) -> &[Entry] {
        &self.rows[usize::from(family == Family::Ipv6)]
    }

    pub(super) fn selection_read(&self, visible: [bool; 2]) -> SelectionRead {
        let mut tables: BTreeMap<u32, Vec<Candidate>> = BTreeMap::new();
        for (entries, visible) in self.rows.iter().zip(visible) {
            if !visible {
                continue;
            }
            for entry in entries.iter().filter(|entry| !entry.route.disabled) {
                if let Some(candidate) = entry.key.candidate(&entry.route) {
                    tables.entry(entry.key.table).or_default().push(candidate);
                }
            }
        }
        SelectionRead {
            revision: self.revision,
            tables,
        }
    }

    pub(super) fn show_selection(&mut self, selection: Option<&Selections>) -> bool {
        let mut changed = false;
        for entry in self
            .rows
            .iter_mut()
            .flatten()
            .filter(|entry| !entry.route.disabled)
        {
            let status = entry
                .key
                .location()
                .and_then(|location| selection?.get(&(entry.key.table, location)).copied());
            if entry.status != status {
                entry.show_status(status);
                changed = true;
            }
        }
        changed
    }
}

pub(super) fn table_name(table: u32) -> String {
    if table == TABLE_MAIN {
        "main".into()
    } else {
        table.to_string()
    }
}

pub(super) fn target_text(target: &StaticTarget) -> String {
    match target {
        StaticTarget::Direct {
            interface, gateway, ..
        } => gateway.map_or_else(
            || interface.as_str().to_owned(),
            |address| format!("{address}%{}", interface.as_str()),
        ),
        StaticTarget::Recursive { address } => address.to_string(),
    }
}

pub(super) fn gateway_text(action: &StaticAction) -> String {
    match action {
        StaticAction::Forward { members } => members
            .iter()
            .map(|member| target_text(&member.target))
            .collect::<Vec<_>>()
            .join(", "),
        StaticAction::Discard => "blackhole".into(),
        StaticAction::Unreachable => "unreachable".into(),
        StaticAction::Prohibit => "prohibit".into(),
    }
}

fn routes_mut<'a>(
    main: &'a mut Vec<StaticRoute>,
    alternate: &'a mut [StaticTable],
    table: u32,
) -> io::Result<&'a mut Vec<StaticRoute>> {
    if table == TABLE_MAIN {
        return Ok(main);
    }
    alternate
        .iter_mut()
        .find(|entry| entry.id == table)
        .map(|entry| &mut entry.routes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "所选路由表已删除。"))
}

/// 在后台以打开条目时的代际生成候选。重新读出的顺序不同时，旧索引不会作用到另一条路由。
pub(super) fn candidate(
    domain: Option<&str>,
    expected: Revision,
    change: Change,
) -> io::Result<(Vec<u8>, Option<Key>)> {
    let (revision, mut config) = read(&mut ManagementClient::local())?;
    if revision != expected {
        return Err(io::Error::other(
            "配置已变化，请关闭属性窗口后从最新列表重新打开。",
        ));
    }
    let (main, policy) = if let Some(domain) = domain {
        let scope = config
            .routing_domains
            .get_mut(domain)
            .ok_or_else(missing_domain)?;
        (&mut scope.routes, &mut scope.policy_routing)
    } else {
        (&mut config.routes, &mut config.policy_routing)
    };
    let key = change.apply(main, &mut policy.tables)?;
    Ok((config.encode().map_err(io::Error::other)?, key))
}

impl Change {
    fn apply(
        self,
        main: &mut Vec<StaticRoute>,
        alternate: &mut [StaticTable],
    ) -> io::Result<Option<Key>> {
        let remove = |routes: &mut Vec<StaticRoute>, index| {
            if index >= routes.len() {
                return Err(missing_route());
            }
            routes.remove(index);
            Ok(())
        };
        match self {
            Self::Put {
                original,
                table,
                route,
            } => {
                if let Some(key) = original {
                    let routes = routes_mut(main, alternate, key.table)?;
                    if key.table == table {
                        *routes.get_mut(key.index).ok_or_else(missing_route)? = route;
                        return Ok(Some(key));
                    }
                    remove(routes, key.index)?;
                }
                let routes = routes_mut(main, alternate, table)?;
                let index = routes.len();
                routes.push(route);
                Ok(Some(Key { table, index }))
            }
            Self::Remove(key) => {
                remove(routes_mut(main, alternate, key.table)?, key.index)?;
                Ok(None)
            }
        }
    }
}

fn missing_route() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选路由已不存在。")
}
