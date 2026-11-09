//! 原生配置的审阅与整份替换；复用授权设置 channel，不新增存储或执行 owner。

use super::*;
use router_protocol::ordinal;

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
    let candidate = if metadata.length != 0 {
        let content = Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?)
            .read_bytes(metadata.length)?;
        Some(Configuration::decode(&content).map_err(|error| {
            warn!("router-config: candidate rejected: {error}");
            native_abi::ERR_INVALID_ARGS
        })?)
    } else {
        None
    };
    // 确认固定字段先预留；IMPORT 持久决定后不再创建 VMO、容器或增长回复。
    reply.try_reserve(24)?;
    let (generation, json) = match operation {
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
