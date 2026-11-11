//! 原生配置的审阅与整份替换；复用授权设置 channel，不新增存储或执行 owner。

use super::*;
use router_protocol::ordinal;
use router_protocol::connections::changes;

fn execution_control(owner: &RouterConfig) -> Result<(u64, RoutingConfigurator), Status> {
    let inner = owner.inner.lock();
    let link = inner.link.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
    let control = link.installer.channel().duplicate(native_abi::rights::SAME)?;
    Ok((inner.generation, RoutingConfigurator::new(control)))
}

pub(super) fn wait_connection_changes(owner: &RouterConfig, bytes: &[u8]) -> Result<(), Status> {
    let cursor = changes::Cursor::parse(bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let (_, control) = execution_control(owner)?;
    if let Some(event) = control.arm_connection_watch(cursor)? {
        // 有界等待使放弃请求的 datagram 客户端不会长期占用等待名额。
        let deadline = native_runtime::system::monotonic().saturating_add(15_000_000_000);
        match event.wait_one(native_abi::signals::USER_0, deadline) {
            Ok(_) | Err(native_abi::ERR_TIMED_OUT) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) fn handle(
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
    let content = if metadata.length != 0 {
        Some(
            Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?)
                .read_bytes(metadata.length)?,
        )
    } else {
        None
    };
    let candidate = if matches!(operation, ordinal::VALIDATE | ordinal::IMPORT | ordinal::DIFF) {
        content
            .as_deref()
            .map(Configuration::decode)
            .transpose()
            .map_err(|error| {
                warn!("router-config: candidate rejected: {error}");
                native_abi::ERR_INVALID_ARGS
            })?
    } else {
        None
    };
    // 确认固定字段先预留；IMPORT 持久决定后不再创建 VMO、容器或增长回复。
    reply.try_reserve(24)?;
    let (generation, json) = match operation {
        ordinal::CONNECTIONS | ordinal::CLEAR_CONNECTION => {
            let selector = content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?;
            let (generation, control) = execution_control(owner)?;
            if operation == ordinal::CLEAR_CONNECTION {
                let identity = router_protocol::connections::Identity::parse(selector)
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                control.clear_connection(identity)?;
                let response = router_protocol::administration::Reply { generation, length: 0 };
                reply.bytes(&response.encode());
                return Ok(Vec::new());
            }
            let cursor = router_protocol::connections::Cursor::parse(selector)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            (generation, control.connections(cursor)?)
        }
        ordinal::CONNECTION_CHANGES => {
            let cursor = changes::Cursor::parse(content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let (generation, control) = execution_control(owner)?;
            (generation, control.connection_changes(cursor)?)
        }
        ordinal::FIREWALL_COUNTERS => {
            let cursor = router_protocol::firewall::counters::Cursor::parse(
                content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?,
            )
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let (generation, control) = execution_control(owner)?;
            (generation, control.firewall_counters(cursor)?)
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
            let (generation, routes) = owner.bgp_routes(query)?;
            (
                generation,
                routes.encode().map_err(|_| native_abi::ERR_BAD_STATE)?,
            )
        }
        ordinal::BGP_STATUS => {
            let (generation, snapshot) = owner.bgp_status()?;
            let json = snapshot.encode().map_err(|_| native_abi::ERR_BAD_STATE)?;
            (generation, json)
        }
        ordinal::BGP_REFRESH => {
            let request = router_protocol::bgp::RefreshRequest::parse(
                content.as_deref().ok_or(native_abi::ERR_INVALID_ARGS)?,
            )
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let generation = owner.bgp_refresh(&request)?;
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
                owner.compile_configuration(&inner, &candidate)?;
                inner.generation
            };
            (
                generation,
                candidate
                    .encode()
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            )
        }
        ordinal::EXPORT | ordinal::DIFF => {
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
            let json = match candidate {
                Some(candidate) => current.difference_json(&candidate),
                None => current.encode(),
            }
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
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
