//! 原生配置树的授权管理客户端；与桌面设置投影共用服务与提交 owner，不维护配置文件。

use super::ManagementClient;
use router_protocol::{Revision, administration, ordinal};
use std::{
    io::{self, Write},
    path::Path,
};

pub use super::gateway::PendingConnectionWatch;
pub use router_protocol::administration::{CommitId, CommitResult};
pub use router_protocol::connections;

pub use router_protocol::backup::{
    Activity as BackupActivity, Id as BackupId, Listing as BackupListing, Policy as BackupPolicy,
};
pub use router_protocol::bgp;
pub use router_protocol::configuration::{
    AdvertisedDns, Assignment, Bridge, BridgePort, Configuration as RouterConfiguration,
    DeviceMatch, DhcpV4Pool, DnsConfiguration, DnsForwarding, DnsListener, DnsUpstream,
    Interface as RouterInterface, Ipv4Configuration, Ipv6Address, Ipv6Configuration,
    Name as ConfigurationName, PolicyRouteRule, PolicyRouting, PrefixDelegation,
    RouterAdvertisementPolicy, RouterAdvertisements, RoutingDomain, RuleOrigin, StaticAction,
    StaticMember, StaticRoute, StaticTable, StaticTarget, VlanEgress, VlanGateway,
    forwarding_cpu_mask, valid_advertised_prefixes, valid_routing_domain_name,
};
pub use router_protocol::configuration::{
    ConnectionState, Firewall, PreroutingAction, PreroutingRule,
};
pub use router_protocol::dhcp_server::Pool as DhcpPool;
pub use router_protocol::firewall::{
    ExistingConnections, NatSource, NatTarget, TransportMatch, TransportProtocol, valid_chain_name,
    valid_name as valid_address_set_name, valid_prefix as valid_firewall_prefix,
};
pub use router_protocol::ospf;
pub use router_protocol::policy::{Disposition as RouteDisposition, Prefix as RoutePolicyPrefix};
pub use router_protocol::policy_routing::{MAIN_PRIORITY as POLICY_MAIN_PRIORITY, alternate_table};
pub use router_protocol::routing::{Attributes as RouteAttributes, Family as RoutingFamily};
pub use router_protocol::switching::{valid_gateway_address, valid_gateway_name, valid_vid};
pub use router_protocol::tracking::IpPrefix;
pub use router_protocol::tracking::routes::Selector as RouteSourceSelector;
pub use router_protocol::{InterfaceState, connection_name, dns_suffix};

/// 返回邻居的管理状态或当前协议状态，供命令行与桌面使用。
pub fn bgp_neighbor_state(peer: &bgp::NeighborStatus) -> &str {
    if peer.shutdown {
        "已停用"
    } else {
        &peer.state
    }
}

/// 将 BGP 的接收路径判定转换为可在命令行和桌面展示的说明。
pub fn bgp_route_decision(decision: bgp::RouteDecision) -> &'static str {
    use bgp::RouteDecision;
    match decision {
        RouteDecision::Pending => "待确认",
        RouteDecision::Selected => "已被 BGP 采用",
        RouteDecision::NotBest => "有更优的 BGP 候选",
        RouteDecision::InvalidPeer => "邻居身份无效",
        RouteDecision::ImportPolicy => "入口策略拒绝或未配置",
        RouteDecision::Unresolved => "下一跳无法满足安装后的可达条件",
        RouteDecision::AsLoop => "AS 路径包含本地 AS",
        RouteDecision::OriginatorLoop => "起源标识返回本机",
        RouteDecision::ClusterLoop => "路由返回本地反射簇",
    }
}

/// 将接收或已选 BGP 路径的属性写成适合审阅的文本。
/// # Errors
/// 输出无法写入。
pub fn write_bgp_path(output: &mut impl Write, attributes: &bgp::PathAttributes) -> io::Result<()> {
    write!(output, "AS 路径：")?;
    for segment in &attributes.path {
        let (start, end) = match segment.kind {
            1 => ("{", "}"),
            3 => ("(", ")"),
            4 => ("[{", "}]"),
            _ => ("", ""),
        };
        write!(output, " {start}")?;
        for (index, asn) in segment.asns.iter().enumerate() {
            if index != 0 {
                write!(output, " ")?;
            }
            write!(output, "{asn}")?;
        }
        write!(output, "{end}")?;
    }
    writeln!(output)?;
    let origin = match attributes.origin {
        0 => "IGP",
        1 => "EGP",
        _ => "Incomplete",
    };
    let metric =
        |value: Option<u32>| value.map_or_else(|| "未设置".into(), |value| value.to_string());
    writeln!(
        output,
        "ORIGIN：{origin}，MED：{}，LOCAL_PREF：{}",
        metric(attributes.med),
        metric(attributes.local_preference)
    )?;
    if !attributes.communities.is_empty() {
        write!(output, "团体属性：")?;
        for community in &attributes.communities {
            write!(output, " {}:{}", community >> 16, community & 0xffff)?;
        }
        writeln!(output)?;
    }
    if !attributes.extended_communities.is_empty() {
        write!(output, "扩展团体属性：")?;
        for community in &attributes.extended_communities {
            write!(output, " {:016x}", u64::from_be_bytes(*community))?;
        }
        writeln!(output)?;
    }
    if !attributes.large_communities.is_empty() {
        write!(output, "大型团体属性：")?;
        for community in &attributes.large_communities {
            let [a, b, c] = community.as_chunks::<4>().0 else {
                unreachable!("12-byte community")
            };
            write!(
                output,
                " {}:{}:{}",
                u32::from_be_bytes(*a),
                u32::from_be_bytes(*b),
                u32::from_be_bytes(*c)
            )?;
        }
        writeln!(output)?;
    }
    Ok(())
}

/// 返回可在命令行和桌面展示的 OSPF 邻居状态名称。
pub fn ospf_neighbor_state(state: ospf::NeighborState) -> &'static str {
    use ospf::NeighborState;
    match state {
        NeighborState::Init => "Init",
        NeighborState::ExStart => "ExStart",
        NeighborState::Exchange => "Exchange",
        NeighborState::Loading => "Loading",
        NeighborState::Full => "Full",
    }
}

/// 将 OSPF 认证拒绝原因转换为用户可读的说明。
pub fn ospf_authentication_failure(failure: ospf::AuthenticationFailure) -> &'static str {
    use ospf::AuthenticationFailure;
    match failure {
        AuthenticationFailure::Missing => "缺少认证数据",
        AuthenticationFailure::Unexpected => "对端启用了认证",
        AuthenticationFailure::Malformed => "认证报文格式无效",
        AuthenticationFailure::UnknownKey => "密钥编号不匹配",
        AuthenticationFailure::InvalidDigest => "签名验证失败",
        AuthenticationFailure::Replay => "报文序列过旧",
    }
}

/// 将配置导出或差异文档写入并同步至新的私有文件。文档可能包含认证密钥。
///
/// # Errors
/// 目标已存在（包括符号链接）、权限不足，或写入、同步失败。失败时尝试移除未完成的文件。
pub fn save_new(path: &Path, document: &[u8]) -> io::Result<()> {
    linux_uapi::filesystem::write_private_new(path, |file| file.write_all(document))
}

/// 为将要发送的导入生成身份。客户端应先保存此值，以便断线后查询。
/// # Errors
/// 系统随机源不可用。
pub fn new_commit_id() -> io::Result<CommitId> {
    use std::io::Read;

    let mut bytes = [0; CommitId::BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(CommitId::from_random_bytes(bytes))
}

impl ManagementClient {
    /// 查询已保存的具名路由域名称，默认域不出现在目录中。
    /// # Errors
    /// 权限不足、配置尚未加载或目录格式无效。
    pub fn routing_domains(&mut self) -> io::Result<Vec<String>> {
        let (_, document) =
            self.configuration_request(ordinal::ROUTING_DOMAINS, None, Default::default())?;
        administration::parse_domain_names(&document)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "路由域目录无效"))
    }

    /// 查询已授权的 OSPF 链路与邻居发现状态。
    /// # Errors
    /// 权限不足、实例尚未应用、实例退出或回复非法。
    pub fn ospf_status(
        &mut self,
        domain: Option<&str>,
    ) -> io::Result<router_protocol::ospf::Snapshot> {
        let (_, json) = self.configuration_request(
            ordinal::OSPF_STATUS,
            None,
            administration::Request {
                domain,
                ..Default::default()
            },
        )?;
        router_protocol::ospf::Snapshot::parse(&json)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "OSPF 状态无效"))
    }

    /// 查询 `domain` 的一页生效路由，None 查询默认域。
    /// # Errors
    /// 域不存在、权限不足、游标或回复无效。
    pub fn route_page(
        &mut self,
        domain: Option<&str>,
        cursor: router_protocol::route_view::Cursor,
    ) -> io::Result<router_protocol::route_view::Page> {
        let (_, bytes) = self.configuration_request(
            ordinal::ROUTE_PAGE,
            Some(&cursor.encode()),
            administration::Request {
                domain,
                ..Default::default()
            },
        )?;
        router_protocol::route_view::Page::parse(&bytes, cursor)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "路由表回复无效"))
    }

    /// 经既有授权配置端点查询 BGP 邻居状态；不直接持有原生工厂或网络执行能力。
    /// # Errors
    /// 尚未应用、权限不足、实例退出或状态正文损坏。
    pub fn bgp_status(
        &mut self,
        domain: Option<&str>,
    ) -> io::Result<router_protocol::bgp::Snapshot> {
        let (_, json) = self.configuration_request(
            ordinal::BGP_STATUS,
            None,
            administration::Request {
                domain,
                ..Default::default()
            },
        )?;
        router_protocol::bgp::Snapshot::parse(&json)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "BGP 状态无效"))
    }

    /// 经授权配置端点向当前邻居连接排队单族 BGP 刷新。成功只表示请求已排队。
    /// # Errors
    /// 邻居名非法、权限不足、会话不可用或未协商所需能力。
    pub fn bgp_refresh(
        &mut self,
        domain: Option<&str>,
        neighbor: &str,
        family: router_protocol::routing::Family,
    ) -> io::Result<()> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "BGP 邻居名称无效");
        let request_body = router_protocol::bgp::RefreshRequest {
            neighbor: neighbor.to_owned().try_into().map_err(|_| invalid())?,
            family,
        }
        .encode()
        .map_err(|_| invalid())?;
        self.configuration_request(
            ordinal::BGP_REFRESH,
            Some(&request_body),
            administration::Request {
                domain,
                ..Default::default()
            },
        )
        .map(|_| ())
    }

    /// 查询一个规范 IPv4 前缀的原始接收、已确认协议选择及未采用原因。
    /// # Errors
    /// 非法前缀、权限不足、服务尚待应用/退出或回复损坏。
    pub fn bgp_routes(
        &mut self,
        domain: Option<&str>,
        query: router_protocol::tracking::IpPrefix,
    ) -> io::Result<router_protocol::bgp::Routes> {
        let json = query
            .encode()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "BGP 前缀无效"))?;
        let (_, json) = self.configuration_request(
            ordinal::BGP_ROUTES,
            Some(&json),
            administration::Request {
                domain,
                ..Default::default()
            },
        )?;
        let routes = router_protocol::bgp::Routes::parse(&json)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "BGP 路由回复无效"))?;
        if routes.query != query {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "BGP 前缀回复不匹配",
            ));
        }
        Ok(routes)
    }

    /// 查询 `domain` 的一页有效转发连接，None 查询默认域。
    /// # Errors
    /// 权限不足、游标失效、服务不可用或分页正文非法。
    pub fn connections(
        &mut self,
        domain: Option<&str>,
        cursor: router_protocol::connections::Cursor,
    ) -> io::Result<router_protocol::connections::Page> {
        let (_, page) = self.configuration_request(
            ordinal::CONNECTIONS,
            Some(&cursor.encode()),
            administration::Request {
                domain,
                ..Default::default()
            },
        )?;
        router_protocol::connections::Page::parse(&page, cursor)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "转发连接列表回复无效"))
    }

    /// 清除 `domain` 中一条已确认的转发连接，None 表示默认域。
    /// # Errors
    /// 身份已失效、无网络管理权限或服务不可用。
    pub fn clear_connection(
        &mut self,
        domain: Option<&str>,
        identity: router_protocol::connections::Identity,
    ) -> io::Result<()> {
        self.configuration_request(
            ordinal::CLEAR_CONNECTION,
            Some(&identity.encode()),
            administration::Request {
                domain,
                ..Default::default()
            },
        )
        .map(drop)
    }

    /// 查询 `domain` 的一页防火墙命中计数，None 查询默认路由域。
    /// # Errors
    /// 权限不足、规则链换版、游标失效或回复无效。
    pub fn firewall_counters(
        &mut self,
        domain: Option<&str>,
        cursor: router_protocol::firewall::counters::Cursor,
    ) -> io::Result<router_protocol::firewall::counters::Page> {
        let (_, bytes) = self.configuration_request(
            ordinal::FIREWALL_COUNTERS,
            Some(&cursor.encode()),
            administration::Request {
                domain,
                ..Default::default()
            },
        )?;
        router_protocol::firewall::counters::Page::parse(&bytes, cursor)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "防火墙计数回复无效"))
    }

    /// 读取 `domain` 中指定位置后的有界增量。首次查询或历史不足时返回重新同步起点。
    /// # Errors
    /// 权限不足、服务不可用或回复无效。
    pub fn connection_changes(
        &mut self,
        domain: Option<&str>,
        cursor: router_protocol::connections::changes::Cursor,
    ) -> io::Result<router_protocol::connections::changes::Batch> {
        let (_, bytes) = self.configuration_request(
            ordinal::CONNECTION_CHANGES,
            Some(&cursor.encode()),
            administration::Request {
                domain,
                ..Default::default()
            },
        )?;
        router_protocol::connections::changes::Batch::parse(&bytes, cursor)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "转发连接增量回复无效"))
    }

    /// 等待 `domain` 中新的连接变化。成功后重新查询游标，也可能是服务端等待到期。
    /// # Errors
    /// 权限不足、等待名额已满或传输失败。
    pub fn wait_connection_changes(
        &mut self,
        domain: Option<&str>,
        cursor: router_protocol::connections::changes::Cursor,
    ) -> io::Result<()> {
        let query = administration::scoped_query(domain, &cursor.encode())
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        let bytes = self.settings_request(
            ordinal::WAIT_CONNECTION_CHANGES,
            &query,
            super::settings::WATCH_TIMEOUT,
        )?;
        if !bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "转发连接等待回复无效",
            ));
        }
        Ok(())
    }

    /// Read a page of leases for the given system interface.
    pub fn dhcp_server_bindings(
        &mut self,
        interface: &[u8],
        after: std::net::Ipv4Addr,
    ) -> io::Result<(
        Revision,
        u64,
        Vec<router_protocol::dhcp_server::Binding>,
        bool,
    )> {
        use router_protocol::dhcp_server;
        let query = dhcp_server::settings_query(interface, after)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "接口名称无效"))?;
        let (generation, page) = self.configuration_request(
            ordinal::DHCP_SERVER_BINDINGS,
            Some(&query),
            Default::default(),
        )?;
        let (observed_ms, leases, more) = dhcp_server::decode_page(&page)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "DHCP 租约回复无效"))?;
        if leases.first().is_some_and(|lease| lease.address <= after) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DHCP 租约游标未推进",
            ));
        }
        Ok((generation, observed_ms, leases, more))
    }

    fn configuration_request(
        &mut self,
        operation: u32,
        document: Option<&[u8]>,
        mut metadata: administration::Request<'_>,
    ) -> io::Result<(Revision, Vec<u8>)> {
        metadata.length = document.map_or(0, |bytes| bytes.len() as u64);
        let control = metadata
            .encode(operation)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "配置请求无效"))?;
        let (reply, body) =
            self.request(operation, &control, document, super::gateway::RPC_TIMEOUT)?;
        let metadata = administration::Reply::decode(operation, &reply)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "管理回复无效"))?;
        Ok((metadata.generation, body))
    }

    /// 导出原生配置 JSON 及其快照代际，不包含租约或运行时转发状态。
    /// # Errors
    /// 无有效配置、权限不足、连接不可用或回复损坏。
    pub fn export(&mut self) -> io::Result<(Revision, Vec<u8>)> {
        self.configuration_request(ordinal::EXPORT, None, Default::default())
    }

    /// 查询同一配置模型生成的 JSON Schema 字段元数据。
    /// # Errors
    /// 权限不足、服务不可用或回复损坏。
    pub fn schema(&mut self) -> io::Result<Vec<u8>> {
        self.configuration_request(ordinal::SCHEMA, None, Default::default())
            .map(|(_, json)| json)
    }

    /// 无副作用校验配置文档及当前设备匹配，返回校验时的代际与当前 JSON 格式的正文。
    /// 不尝试运行 DHCP，也不保证将来的资源预备或持久化一定成功。
    /// # Errors
    /// 候选无效、匹配冲突、权限不足或服务不可用。
    pub fn validate(&mut self, document: &[u8]) -> io::Result<(Revision, Vec<u8>)> {
        self.configuration_request(ordinal::VALIDATE, Some(document), Default::default())
    }

    /// 完整替换配置。传入已审阅的快照代际及为此次提交生成的身份，成功返回新代际。
    /// 断线、超时或应用状态未确认时，用原身份查询保存结果。
    /// # Errors
    /// 代际已变、配置无效、执行/持久化失败、补偿未确认、无权限或传输失败。
    pub fn import(
        &mut self,
        generation: Revision,
        commit: CommitId,
        document: &[u8],
    ) -> io::Result<Revision> {
        let metadata = administration::Request {
            generation: Some(generation),
            commit: Some(commit),
            ..Default::default()
        };
        self.configuration_request(ordinal::IMPORT, Some(document), metadata)
            .map(|(generation, _)| generation)
    }

    /// 查询此次导入的持久结果。旧记录已退出保留窗口时返回 `Unknown`。
    /// # Errors
    /// 权限不足、配置存储状态未确认、服务不可用或回复无效。
    pub fn commit_result(&mut self, id: CommitId) -> io::Result<CommitResult> {
        let metadata = administration::Request {
            commit: Some(id),
            ..Default::default()
        };
        let (_, body) = self.configuration_request(ordinal::COMMIT_RESULT, None, metadata)?;
        CommitResult::decode(&body)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "配置提交结果无效"))
    }

    /// 返回审阅时的代际和 JSON 变更列表；每项含 JSON Pointer path 以及存在时的 before/after。
    /// 有序数组跳过相同的首尾元素，再按位置比较。没有已保存配置时返回待恢复的内容。
    /// # Errors
    /// 候选配置或当前设备匹配无效、权限不足、资源不足或服务不可用。
    pub fn difference(&mut self, document: &[u8]) -> io::Result<(Revision, Vec<u8>)> {
        self.configuration_request(ordinal::DIFF, Some(document), Default::default())
    }

    /// 查询可导出的备份及自动任务实际状态；不缓存文件索引。
    /// # Errors
    /// 权限、存储、传输或回复格式失败。
    pub fn backups(&mut self) -> io::Result<BackupListing> {
        let (_, json) = self.configuration_request(ordinal::BACKUPS, None, Default::default())?;
        BackupListing::decode(&json)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// 创建手动备份；自动保留策略不会删除它。结果不确定时先刷新备份列表，不盲目重放。
    /// # Errors
    /// 无有效配置、权限、存储、资源或传输失败。
    pub fn create_backup(&mut self) -> io::Result<BackupId> {
        let (_, json) =
            self.configuration_request(ordinal::BACKUP_CREATE, None, Default::default())?;
        router_protocol::backup::decode_id(&json)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// 导出所选备份的原始配置 JSON；损坏内容仍可提取，恢复时必须经完整配置校验。
    /// # Errors
    /// 身份不存在/含糊、权限、存储或传输失败。
    pub fn export_backup(&mut self, id: BackupId) -> io::Result<Vec<u8>> {
        let metadata = administration::Request {
            backup: Some(id),
            ..Default::default()
        };
        self.configuration_request(ordinal::BACKUP_EXPORT, None, metadata)
            .map(|(_, json)| json)
    }

    /// 显式删除并持久确认一个备份；错误不证明文件仍存在。
    /// # Errors
    /// 身份不存在/含糊、权限、存储或传输失败。
    pub fn delete_backup(&mut self, id: BackupId) -> io::Result<()> {
        let metadata = administration::Request {
            backup: Some(id),
            ..Default::default()
        };
        self.configuration_request(ordinal::BACKUP_DELETE, None, metadata)
            .map(drop)
    }

    /// 恢复复用唯一完整导入，不绕过校验、预期代际或整批执行/回滚。
    /// # Errors
    /// 备份读取、配置校验、代际冲突或提交失败；结果不确定先重新查询。
    pub fn restore_backup(
        &mut self,
        generation: Revision,
        id: BackupId,
        commit: CommitId,
    ) -> io::Result<Revision> {
        let document = self.export_backup(id)?;
        self.import(generation, commit, &document)
    }
}
