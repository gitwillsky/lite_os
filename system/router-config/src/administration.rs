//! 原生配置的审阅与整份替换；复用授权设置 channel，不新增存储或执行 owner。

use super::*;
use router_protocol::connections::changes;
use router_protocol::ordinal;

fn require_domain(inner: &Inner, name: &str) -> Result<(), Status> {
    let saved = inner.saved.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
    if !saved.routing_domains.contains_key(name) {
        return Err(native_abi::ERR_NOT_FOUND);
    }
    Ok(())
}

fn execution_control(
    owner: &RouterConfig,
    domain: Option<&str>,
) -> Result<(Revision, RoutingConfigurator), Status> {
    let inner = owner.inner.lock();
    let installer = if let Some(name) = domain {
        require_domain(&inner, name)?;
        inner.domains.controller(name)?
    } else {
        &inner
            .link
            .as_ref()
            .ok_or(native_abi::ERR_BAD_STATE)?
            .installer
    };
    let control = installer.channel().duplicate(native_abi::rights::SAME)?;
    Ok((inner.generation, RoutingConfigurator::new(control)))
}

fn namespace_control(
    owner: &RouterConfig,
    domain: Option<&str>,
) -> Result<(Revision, RemoteNamespace), Status> {
    let inner = owner.inner.lock();
    let network = if let Some(name) = domain {
        require_domain(&inner, name)?;
        inner.domains.network(name)?
    } else {
        &inner
            .link
            .as_ref()
            .ok_or(native_abi::ERR_BAD_STATE)?
            .network
    };
    Ok((inner.generation, network.duplicate()?))
}

fn wait_connection_changes(owner: &RouterConfig, bytes: &[u8]) -> Result<(), Status> {
    let (domain, query) = router_protocol::administration::parse_scoped_query(bytes)
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let cursor = changes::Cursor::parse(query).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let (_, control) = execution_control(owner, domain)?;
    if let Some(event) = control.arm_connection_watch(cursor)? {
        // 有界等待使放弃请求的 datagram 客户端不会长期占用等待名额。
        let deadline = native_runtime::system::monotonic()
            .saturating_add(router_protocol::bridge::WATCH_SECONDS as i64 * 1_000_000_000);
        match event.wait_one(native_abi::signals::USER_0, deadline) {
            Ok(_) | Err(native_abi::ERR_TIMED_OUT) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn handle(
    owner: &RouterConfig,
    operation: u32,
    request: &mut service_rpc::Reader<'_>,
    mut handles: Vec<native_runtime::Handle>,
    reply: &mut service_rpc::Writer,
) -> Result<Vec<native_runtime::Handle>, Status> {
    let metadata = router_protocol::administration::Request::decode(operation, request.bytes()?)
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    request.finish()?;
    if handles.len() != usize::from(metadata.length != 0) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    if operation == ordinal::OPEN_NETWORK_NAMESPACE {
        let name = metadata.domain.ok_or(native_abi::ERR_INVALID_ARGS)?;
        let inner = owner.inner.lock();
        require_domain(&inner, name)?;
        let (instance, network) = inner.domains.namespace(name)?;
        let mut returned = Vec::new();
        returned
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        reply.try_reserve(8 + router_protocol::administration::Reply::BYTES + 16)?;
        returned.push(network.duplicate()?.into_channel().into_handle());
        reply.bytes(
            &router_protocol::administration::Reply {
                generation: inner.generation,
                length: 0,
            }
            .encode(),
        );
        reply.bytes(&instance.get().to_le_bytes());
        return Ok(returned);
    }
    let mut content = if metadata.length != 0 {
        Some(
            Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?)
                .read_bytes(metadata.length)?,
        )
    } else {
        None
    };
    let candidate = if matches!(
        operation,
        ordinal::VALIDATE | ordinal::IMPORT | ordinal::DIFF
    ) {
        // 解码结束即释放传入正文。审阅与提交只持当前模型，避免大文档在生成差异时留存两份。
        content
            .take()
            .as_deref()
            .map(policy::decode_document)
            .transpose()
            .map_err(|error| {
                warn!("router-config: candidate rejected: {error}");
                native_abi::ERR_INVALID_ARGS
            })?
    } else {
        None
    };
    // 确认固定字段先预留；IMPORT 持久决定后不再创建 VMO、容器或增长回复。
    reply.try_reserve(4 + router_protocol::administration::Reply::BYTES)?;
    let (generation, json) = match operation {
        ordinal::CONNECTIONS | ordinal::CLEAR_CONNECTION => {
            let selector = content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?;
            let (generation, control) = execution_control(owner, metadata.domain)?;
            if operation == ordinal::CLEAR_CONNECTION {
                let identity = router_protocol::connections::Identity::parse(selector)
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                control.clear_connection(identity)?;
                let response = router_protocol::administration::Reply {
                    generation,
                    length: 0,
                };
                reply.bytes(&response.encode());
                return Ok(Vec::new());
            }
            let cursor = router_protocol::connections::Cursor::parse(selector)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            (generation, control.connections(cursor)?)
        }
        ordinal::CONNECTION_CHANGES => {
            let cursor =
                changes::Cursor::parse(content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?)
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let (generation, control) = execution_control(owner, metadata.domain)?;
            (generation, control.connection_changes(cursor)?)
        }
        ordinal::FIREWALL_COUNTERS => {
            let cursor = router_protocol::firewall::counters::Cursor::parse(
                content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?,
            )
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let (generation, control) = execution_control(owner, metadata.domain)?;
            (generation, control.firewall_counters(cursor)?)
        }
        ordinal::NEIGHBOR_PAGE => {
            let after = router_protocol::neighbors::parse_cursor(
                content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?,
            )
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let (generation, namespace) = namespace_control(owner, metadata.domain)?;
            (
                generation,
                namespace
                    .neighbor_page(after)
                    .map_err(configuration_status)?,
            )
        }
        ordinal::ROUTE_PAGE => {
            let cursor = router_protocol::route_view::Cursor::parse(
                content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?,
            )
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let (generation, namespace) = namespace_control(owner, metadata.domain)?;
            (
                generation,
                namespace.route_page(cursor).map_err(configuration_status)?,
            )
        }
        ordinal::ROUTING_DOMAINS => {
            let (generation, saved) = {
                let inner = owner.inner.lock();
                (
                    inner.generation,
                    inner.saved.clone().ok_or(native_abi::ERR_BAD_STATE)?,
                )
            };
            let names = router_protocol::administration::domain_names(
                saved
                    .routing_domains
                    .keys()
                    .map(router_protocol::configuration::Name::as_str),
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            (generation, names)
        }
        ordinal::DHCP_SERVER_BINDINGS => {
            let (name, after) = router_protocol::dhcp_server::decode_settings_query(
                content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?,
            )
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            owner.pool_bindings(name, after)?
        }
        ordinal::BGP_ROUTES => {
            let query = router_protocol::tracking::IpPrefix::parse(
                content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?,
            )
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let (generation, routes) = owner.bgp_routes(metadata.domain, query)?;
            (
                generation,
                routes.encode().map_err(|_| native_abi::ERR_BAD_STATE)?,
            )
        }
        ordinal::BGP_STATUS => {
            let (generation, snapshot) = owner.bgp_status(metadata.domain)?;
            let json = snapshot.encode().map_err(|_| native_abi::ERR_BAD_STATE)?;
            (generation, json)
        }
        ordinal::OSPF_STATUS => {
            let (generation, snapshot) = owner.ospf_status(metadata.domain)?;
            let json = snapshot.encode().map_err(|_| native_abi::ERR_BAD_STATE)?;
            (generation, json)
        }
        ordinal::BGP_REFRESH => {
            let request = router_protocol::bgp::RefreshRequest::parse(
                content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?,
            )
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let generation = owner.bgp_refresh(metadata.domain, &request)?;
            reply.bytes(
                &router_protocol::administration::Reply {
                    generation,
                    length: 0,
                }
                .encode(),
            );
            return Ok(Vec::new());
        }
        ordinal::BACKUPS => {
            let json = owner.backup_list()?;
            let generation = owner.inner.lock().generation;
            (generation, json)
        }
        ordinal::BACKUP_CREATE => {
            let (generation, id) = owner.backup_create()?;
            (
                generation,
                router_protocol::backup::encode_id(id).map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            )
        }
        ordinal::BACKUP_EXPORT => {
            let bytes =
                owner.backup_export(metadata.backup.expect("backup export has identity"))?;
            let generation = owner.inner.lock().generation;
            (generation, bytes)
        }
        ordinal::BACKUP_DELETE => {
            let generation =
                owner.backup_delete(metadata.backup.expect("backup delete has identity"))?;
            reply.bytes(
                &router_protocol::administration::Reply {
                    generation,
                    length: 0,
                }
                .encode(),
            );
            return Ok(Vec::new());
        }
        ordinal::IMPORT => {
            let generation = owner.replace(
                metadata.generation.expect("IMPORT has generation"),
                metadata.commit.expect("IMPORT has identity"),
                candidate.expect("IMPORT has candidate"),
            )?;
            let response = router_protocol::administration::Reply {
                generation,
                length: 0,
            }
            .encode();
            reply.bytes(&response);
            return Ok(Vec::new());
        }
        ordinal::COMMIT_RESULT => {
            use router_protocol::administration::CommitResult;

            let inner = owner.inner.lock();
            if inner.saved.is_none() || inner.rollback.is_some() {
                return Err(native_abi::ERR_BAD_STATE);
            }
            let id = metadata.commit.expect("COMMIT_RESULT has identity");
            let result = if inner.committed_imports.contains(&id) {
                CommitResult::Saved
            } else {
                CommitResult::Unknown
            };
            let mut body = Vec::new();
            body.try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            body.push(result as u8);
            (inner.generation, body)
        }
        ordinal::SCHEMA => {
            let generation = owner.inner.lock().generation;
            (
                generation,
                Configuration::schema_json().map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            )
        }
        ordinal::VALIDATE => {
            let candidate = candidate.expect("VALIDATE has candidate");
            let generation = {
                let inner = owner.inner.lock();
                owner.project_interfaces(&inner, &candidate)?;
                inner.generation
            };
            (
                generation,
                candidate
                    .encode()
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            )
        }
        ordinal::DIFF => {
            let candidate = candidate.expect("DIFF has candidate");
            let (generation, current) = {
                let inner = owner.inner.lock();
                owner.project_interfaces(&inner, &candidate)?;
                (inner.generation, inner.saved.clone())
            };
            (
                generation,
                Configuration::difference_json(current.as_deref(), &candidate)
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            )
        }
        ordinal::EXPORT => {
            let (generation, current) = {
                let inner = owner.inner.lock();
                (
                    inner.generation,
                    inner
                        .saved
                        .as_ref()
                        .ok_or(native_abi::ERR_BAD_STATE)?
                        .clone(),
                )
            };
            let json = current.encode().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            (generation, json)
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    };
    let content = Vmo::from_bytes(&json)?;
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    returned.push(content.into_handle());
    let response = router_protocol::administration::Reply {
        generation,
        length: json.len() as u64,
    }
    .encode();
    reply.bytes(&response);
    Ok(returned)
}

/// 接收已由 Linux 侧网关授权的设置与观察请求。
pub(super) fn serve(router_config: Arc<RouterConfig>, channel: Channel) -> Result<(), Status> {
    service_rpc::serve(
        channel,
        2,
        b"router-config-settings",
        Arc::new(
            move |ordinal: u32,
                  request: &mut service_rpc::Reader<'_>,
                  handles: Vec<native_runtime::Handle>,
                  reply: &mut service_rpc::Writer| {
                if router_protocol::ordinal::is_configuration(ordinal) {
                    return handle(&router_config, ordinal, request, handles, reply);
                }
                if !handles.is_empty() {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                match ordinal {
                    router_protocol::ordinal::GET => {
                        request.finish()?;
                        let mut encoder = Encoder::default();
                        encoder.snapshot(&RouterConfig::snapshot(&router_config.inner.lock())?);
                        reply.bytes(&encoder.0);
                    }
                    router_protocol::ordinal::SET => {
                        let mut decoder = Decoder::new(request.bytes()?);
                        request.finish()?;
                        let update = decoder.update().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        decoder.finish().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        router_config.set(update)?;
                    }
                    router_protocol::ordinal::WAIT => {
                        let generation = Revision::from_bytes(request.bytes()?)
                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        request.finish()?;
                        reply.bytes(&router_config.wait(generation)?.to_bytes());
                    }
                    router_protocol::ordinal::WAIT_CONNECTION_CHANGES => {
                        let cursor = request.bytes()?;
                        request.finish()?;
                        wait_connection_changes(&router_config, cursor)?;
                    }
                    router_protocol::ordinal::DHCP => {
                        let name = request.bytes()?;
                        let action = DhcpAction::decode(request.u8()?)
                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        request.finish()?;
                        router_config.dhcp_command(name, action)?;
                    }
                    router_protocol::ordinal::RENAME => {
                        let interface = request.bytes()?;
                        let name = request.bytes()?;
                        request.finish()?;
                        router_config.rename(interface, name)?;
                    }
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            },
        ),
    )
}
