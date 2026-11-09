//! 原生配置树的授权管理客户端；与桌面设置投影共用服务与提交 owner，不维护配置文件。

use router_protocol::{administration, ordinal};
use std::{io, time::Duration};

pub use router_protocol::configuration::Configuration;

/// 单次审阅或提交的控制面等待上限；超时/断线不能证明修改未发生，调用方应重新查询。
const TIMEOUT: Duration = Duration::from_secs(30);

fn request(
    operation: u32,
    generation: Option<u64>,
    json: Option<&[u8]>,
    backup: Option<router_protocol::backup::Id>,
) -> io::Result<(u64, Vec<u8>)> {
    let metadata = administration::Request {
        backup,
        generation,
        length: json.map_or(0, |json| json.len() as u64),
    };
    let metadata = metadata
        .encode(operation)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "配置请求无效"))?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (metadata, TIMEOUT);
        Err(io::ErrorKind::Unsupported.into())
    }
    #[cfg(target_os = "linux")]
    {
        use linux_uapi::{
            shared_memory::MemFd,
            unix::{anonymous_datagram, recv_fds, send_fd},
        };
        use std::{
            ffi::OsStr,
            fs::File,
            os::{
                fd::AsFd,
                unix::{ffi::OsStrExt, fs::FileExt},
            },
        };
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "原生配置回复无效");
        let socket = anonymous_datagram()?;
        socket.set_read_timeout(Some(TIMEOUT))?;
        socket.set_write_timeout(Some(TIMEOUT))?;
        socket.connect(OsStr::from_bytes(router_protocol::bridge::PATH))?;
        let memory = json
            .map(|bytes| MemFd::from_bytes("router-configuration", bytes))
            .transpose()?;
        let mut frame = Vec::with_capacity(4 + metadata.len());
        frame.extend_from_slice(&operation.to_le_bytes());
        frame.extend_from_slice(&metadata);
        let sent = match &memory {
            Some(memory) => send_fd(socket.as_fd(), &frame, memory.as_fd())?,
            None => socket.send(&frame)?,
        };
        if sent != frame.len() {
            return Err(io::ErrorKind::WriteZero.into());
        }
        let mut response = [0; 24];
        let (count, files) = recv_fds(socket.as_fd(), &mut response)?;
        if count < 4 {
            return Err(invalid());
        }
        let status = i32::from_le_bytes(response[..4].try_into().expect("four status bytes"));
        if status != 0 {
            if count != 4 || !files.is_empty() {
                return Err(invalid());
            }
            return Err(io::Error::other(format!(
                "网络设置服务拒绝请求（状态 {status}）"
            )));
        }
        let metadata =
            administration::Reply::decode(operation, &response[4..count]).map_err(|_| invalid())?;
        if files.len() != usize::from(administration::Reply::has_document(operation)) {
            return Err(invalid());
        }
        if !administration::Reply::has_document(operation) {
            return Ok((metadata.generation, Vec::new()));
        }
        let [file] = <[_; 1]>::try_from(files).map_err(|_| invalid())?;
        let file = File::from(file);
        let info = file.metadata()?;
        if !info.is_file() || info.len() != metadata.length {
            return Err(invalid());
        }
        let length = usize::try_from(metadata.length).map_err(|_| invalid())?;
        let mut document = Vec::new();
        document
            .try_reserve_exact(length)
            .map_err(|_| io::ErrorKind::OutOfMemory)?;
        document.resize(length, 0);
        file.read_exact_at(&mut document, 0)?;
        Ok((metadata.generation, document))
    }
}

/// 导出完整持久配置及其快照代际，不包含租约或运行时转发状态。
/// # Errors
/// 无有效配置、权限不足、连接不可用或回复损坏。
pub fn export() -> io::Result<(u64, Configuration)> {
    let (generation, json) = request(ordinal::EXPORT, None, None, None)?;
    Configuration::decode(&json)
        .map(|configuration| (generation, configuration))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// 查询同一配置模型生成的 JSON Schema 字段元数据。
/// # Errors
/// 权限不足、服务不可用或回复损坏。
pub fn schema() -> io::Result<Vec<u8>> {
    request(ordinal::SCHEMA, None, None, None).map(|(_, json)| json)
}

/// 无副作用校验 JSON 语法、字段语义及当前设备匹配，返回校验时的代际与规范化候选。
/// 不尝试运行 DHCP，也不保证将来的资源预备或持久化一定成功。
/// # Errors
/// 候选无效、匹配冲突、权限不足或服务不可用。
pub fn validate(json: &[u8]) -> io::Result<(u64, Configuration)> {
    let (generation, normalized) = request(ordinal::VALIDATE, None, Some(json), None)?;
    Configuration::decode(&normalized)
        .map(|configuration| (generation, configuration))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// 完整替换配置，包括恢复加载失败的配置；传入最近读取的快照代际，成功返回提交后的代际。
/// 断线/超时或 BAD_STATE 后先重新查询，不能盲目重放。代际只在当前配置服务实例内有意义。
/// # Errors
/// 代际已变、配置无效、执行/持久化失败、补偿未确认、无权限或传输失败。
pub fn import(generation: u64, json: &[u8]) -> io::Result<u64> {
    request(ordinal::IMPORT, Some(generation), Some(json), None).map(|(generation, _)| generation)
}

/// 返回审阅时的代际和 JSON 变更列表；每项含 JSON Pointer path 以及存在时的 before/after。
/// 有序数组作为整体比较；输出只用于审阅，不是另一条修改入口。
/// # Errors
/// 当前/候选配置无效、权限不足、资源不足或服务不可用。
pub fn difference(json: &[u8]) -> io::Result<(u64, Vec<u8>)> {
    request(ordinal::DIFF, None, Some(json), None)
}

pub use router_protocol::backup::{Id as BackupId, Listing as BackupListing};

/// 查询可导出的备份及自动任务实际状态；不缓存文件索引。
/// # Errors
/// 权限、存储、传输或回复格式失败。
pub fn backups() -> io::Result<BackupListing> {
    let (_, json) = request(ordinal::BACKUPS, None, None, None)?;
    BackupListing::decode(&json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// 创建手动备份；自动保留策略不会删除它。结果不确定时先刷新备份列表，不盲目重放。
/// # Errors
/// 无有效配置、权限、存储、资源或传输失败。
pub fn create_backup() -> io::Result<BackupId> {
    let (_, json) = request(ordinal::BACKUP_CREATE, None, None, None)?;
    router_protocol::backup::decode_id(&json)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// 导出所选备份的原始配置 JSON；损坏内容仍可提取，恢复时必须经完整配置校验。
/// # Errors
/// 身份不存在/含糊、权限、存储或传输失败。
pub fn export_backup(id: BackupId) -> io::Result<Vec<u8>> {
    request(ordinal::BACKUP_EXPORT, None, None, Some(id)).map(|(_, json)| json)
}

/// 显式删除并持久确认一个备份；错误不证明文件仍存在。
/// # Errors
/// 身份不存在/含糊、权限、存储或传输失败。
pub fn delete_backup(id: BackupId) -> io::Result<()> {
    request(ordinal::BACKUP_DELETE, None, None, Some(id)).map(drop)
}

/// 恢复复用唯一完整导入，不绕过校验、预期代际或整批执行/回滚。
/// # Errors
/// 备份读取、配置校验、代际冲突或提交失败；结果不确定先重新查询。
pub fn restore_backup(generation: u64, id: BackupId) -> io::Result<u64> {
    import(generation, &export_backup(id)?)
}
