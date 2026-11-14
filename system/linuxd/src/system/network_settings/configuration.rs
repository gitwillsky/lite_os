//! 配置文档的标准 fd ↔ 原生 VMO 投影；控制帧不承载正文，也不维护业务状态。

use super::*;
use crate::{
    file::{OpenFileDescription, PreparedFileDescription},
    fs::{
        AccessIdentity, FileSystemError, InodeOpen, InodeType, O_ACCMODE, O_RDONLY, O_WRONLY,
        SharedMemoryFile,
    },
    memory::MemoryObject,
    net::{UnixPassedFile, UnixReceivedRights, UnixRights},
};
use native_runtime::Vmo;
use router_protocol::administration::{Reply, Request};

fn filesystem_status(error: FileSystemError) -> Status {
    match error {
        FileSystemError::OutOfMemory => native_abi::ERR_NO_MEMORY,
        FileSystemError::PermissionDenied => native_abi::ERR_ACCESS_DENIED,
        _ => native_abi::ERR_IO,
    }
}

/// 在任何转发/重放之前固定输入；OFD 的当前位置不影响整个文档，绝不重新打开用户路径。
pub(super) fn prepare(
    operation: u32,
    payload: &[u8],
    rights: Option<UnixReceivedRights>,
) -> Result<Option<Vmo>, Status> {
    let metadata = Request::decode(operation, payload).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    if metadata.length == 0 {
        return if rights.is_none() {
            Ok(None)
        } else {
            Err(native_abi::ERR_INVALID_ARGS)
        };
    }
    let files = rights.ok_or(native_abi::ERR_INVALID_ARGS)?.into_files();
    let [file] = <[_; 1]>::try_from(files).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let file = file
        .into_any()
        .downcast::<OpenFileDescription>()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    if file.is_path() || file.status_flags() & O_ACCMODE == O_WRONLY {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let inode = file.inode_ref();
    let info = inode.metadata().map_err(filesystem_status)?;
    if info.kind != InodeType::File || info.size != metadata.length {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    // 普通文件也能作为输入；固定的小缓冲捕获使远端请求不依赖发送方 fd/pager 的后续寿命。
    file.regular_file()
        .map_err(filesystem_status)?
        .snapshot_vmo(metadata.length)
        .map(Some)
        .map_err(filesystem_status)
}

/// 请求的快照 VMO 保持在调用者，重连查询只重复同一份已固定输入，不再次读取可变文件。
pub(super) fn forward(
    channel: &Channel,
    operation: u32,
    payload: &[u8],
    input: Option<&Vmo>,
) -> Result<(Vec<u8>, Option<UnixRights>), Status> {
    let mut request = service_rpc::Writer::request(operation);
    request.bytes(payload);
    let mut handles = Vec::new();
    if let Some(input) = input {
        handles
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(input.duplicate(native_abi::rights::SAME)?.into_handle());
    }
    let mut response = service_rpc::call(channel, &request, handles)?;
    let mut reader = response.reader();
    let header = reader.bytes()?.to_vec();
    let metadata = Reply::decode(operation, &header).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    if operation == ordinal::OPEN_NETWORK_NAMESPACE {
        let instance = core::num::NonZeroU128::new(u128::from_le_bytes(
            reader
                .bytes()?
                .try_into()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
        ))
        .ok_or(native_abi::ERR_INVALID_ARGS)?;
        reader.finish()?;
        if response.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let network = network_protocol::RemoteNamespace::from_channel(Channel::from_handle(
            service_rpc::take_handle(&mut response.handles, 0)?,
        ));
        let namespace = crate::net::NetNamespace::adopt_managed(instance, network)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let reference = crate::namespace::NamespaceReference::new(namespace);
        let file = OpenFileDescription::open_namespace(reference).map_err(filesystem_status)?;
        return Ok((header, Some(single_file(file)?)));
    }
    reader.finish()?;
    if response.handles.len() != usize::from(Reply::has_document(operation)) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    if !Reply::has_document(operation) {
        return Ok((header, None));
    }
    let content = Vmo::from_handle(service_rpc::take_handle(&mut response.handles, 0)?);
    if metadata.length > content.size()? {
        return Err(native_abi::ERR_OUT_OF_RANGE);
    }
    let object = MemoryObject::new(content).map_err(|error| {
        if error.is_out_of_memory() {
            native_abi::ERR_NO_MEMORY
        } else {
            native_abi::ERR_IO
        }
    })?;
    let opened = SharedMemoryFile::create_opened(
        b"/router-config (deleted)".to_vec(),
        metadata.length,
        object,
        &AccessIdentity::root(),
    )
    .map_err(filesystem_status)?;
    let prepared = PreparedFileDescription::try_new()
        .map_err(filesystem_status)?
        .prepare_named(opened, O_RDONLY)
        .map_err(filesystem_status)?;
    let file = prepared.finish(InodeOpen::Storage(None));
    Ok((header, Some(single_file(file)?)))
}

fn single_file(file: Arc<OpenFileDescription>) -> Result<UnixRights, Status> {
    let mut files: Vec<Arc<dyn UnixPassedFile>> = Vec::new();
    files
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    files.push(file);
    UnixRights::new(files, 0, None).map_err(|error| match error {
        crate::net::SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        _ => native_abi::ERR_INVALID_ARGS,
    })
}
