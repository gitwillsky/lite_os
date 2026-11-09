//! 仅接口配置 owner 得到的执行事务能力；沿现有安装授权派生，不向 Linux 域 channel 开放。

use alloc::vec::Vec;
use native_runtime::{
    Channel, Handle,
    native_abi::{self, Status},
};
use service_rpc::{Reader, Writer};

use super::SocketError;

mod batch;
pub use batch::ConfigurationBatch;

const ACTIVATE: u32 = 1;
const CONFIRM: u32 = 2;
const ABORT: u32 = 3;

/// 执行服务的在途配置：未确认前必须持有可无分配恢复的旧资源。
pub trait ConfigurationService: Send + Sync {
    /// 原子发布预备好的批次。失败不允许半次发布。
    /// # Errors
    /// 事务无效或设备已消失。
    fn activate(&self) -> Result<(), SocketError>;
    /// 确认发布并释放旧资源与配置写租约。
    /// # Errors
    /// 未发布或事务无效；已发布后的设备寿命不改变确认决定。
    fn confirm(&self) -> Result<(), SocketError>;
    /// 撤销发布并释放写租约；其他存活资源必须先恢复，再报告无法恢复的设备。
    /// # Errors
    /// 事务无效或设备已消失。
    fn abort(&self) -> Result<(), SocketError>;
}

/// 分派事务请求；拒绝附带数据和句柄，错误通过同一网络领域结果编码返回。
///
/// # Errors
/// 请求畸形或未知操作。
pub fn dispatch_configuration(
    service: &dyn ConfigurationService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    request.finish()?;
    if !handles.is_empty() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    // 成功/领域错误均编码为一个字节；在激活/确认副作用前准备，不能发布后再因回复扩容失败退出。
    reply.try_reserve(1)?;
    let result = match ordinal {
        ACTIVATE => service.activate(),
        CONFIRM => service.confirm(),
        ABORT => service.abort(),
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    };
    super::put_result(reply, result);
    Ok(Vec::new())
}

/// 客户端持有的事务 channel；关闭自动撤销未确认配置，不需 Drop 时追加 RPC。
pub struct PreparedConfiguration(Channel);

impl PreparedConfiguration {
    pub(super) fn new(channel: Channel) -> Self {
        Self(channel)
    }

    fn command(&self, ordinal: u32) -> Result<(), SocketError> {
        let reply = service_rpc::call(&self.0, &Writer::request(ordinal), Vec::new())
            .map_err(super::client::transport)?;
        if !reply.handles.is_empty() {
            return Err(SocketError::Invalid);
        }
        let mut reader = reply.reader();
        let result = super::get_result(&mut reader).map_err(super::client::transport)?;
        reader.finish().map_err(|_| SocketError::Invalid)?;
        result
    }

    /// 原子激活批次；重复激活同一事务无副作用，确认前可回滚。
    /// # Errors
    /// 设备消失、事务无效或执行服务不可用。
    pub fn activate(&self) -> Result<(), SocketError> {
        self.command(ACTIVATE)
    }

    /// 确认当前配置并消费能力，执行服务释放原节点。
    /// # Errors
    /// 未激活、事务无效或服务不可用；传输失败不证明服务尚未执行确认。
    pub fn confirm(self) -> Result<(), SocketError> {
        self.command(CONFIRM)
    }

    /// 同步撤销并消费能力；可用来等待回滚完成，而非仅等待 channel 关闭通知。
    /// # Errors
    /// 设备已消失、事务无效或服务不可用。
    pub fn abort(self) -> Result<(), SocketError> {
        self.command(ABORT)
    }
}
