//! socket 的共享数据面；由现有 netstack dispatcher 消费，不创建逐 socket 转发线程。
use super::*;
use native_runtime::native_abi::{rights, signals};
use native_runtime::sync::Mutex;
use queue::{CAPACITY, Queue};
struct State {
    last: [u64; CAPACITY],
    scratch: Vec<u8>,
    // OWNER: 仅解码当前请求的有序控制项；首次实际使用准备并复用，控制与数据操作仍由同一 socket 锁串行。
    controls: Vec<InternetSendPacketInfo>,
    // FLAG: 只交出一次 producer capability；重复协商会产生两个不协调的客户端。
    negotiated: bool,
}
/// 服务端共享 SQ/CQ。固定槽只在 metadata 页创建时分配；载荷按实际访问惰性提交。
///
/// 服务端必须串行调用 `drain` 与控制操作，保证同一 socket 的协议顺序；异步等待属于调用者。
pub struct SocketQueue {
    ring: Queue,
    ready: Event,
    progress: Event,
    // OWNER: 唯一 SQ consumer/CQ producer 及发送暂存；ticket 防止已处理的槽身份被重放。
    state: Mutex<State>,
}
impl SocketQueue {
    /// 创建固定映射与事件；内存或原生对象不足时返回相应错误。
    pub fn create() -> Result<Self, Status> {
        Ok(Self {
            ring: Queue::create()?,
            ready: Event::create()?,
            progress: Event::create()?,
            state: Mutex::new(State {
                last: [0; CAPACITY],
                scratch: Vec::new(),
                controls: Vec::new(),
                negotiated: false,
            }),
        })
    }
    /// 候选请求的电平通知；先清除再 `drain`，重武装后不会漏掉并发发表。
    pub fn requests(&self) -> &Event {
        &self.ready
    }
    /// 处理一次控制面协商；未知方法返回 `None`，重复协商或句柄准备失败返回错误。
    pub fn negotiate(
        &self,
        ordinal: u32,
        reply: &mut Writer,
    ) -> Option<Result<Vec<Handle>, Status>> {
        if ordinal != ordinal::QUEUE {
            return None;
        }
        Some((|| {
            let mut state = self.state.lock();
            if state.negotiated {
                return Err(native_abi::ERR_ALREADY_BOUND);
            }
            let mut handles = Vec::new();
            handles
                .try_reserve_exact(3)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            handles.push(
                self.ring
                    .vmo()
                    .duplicate(
                        rights::READ
                            | rights::WRITE
                            | rights::MAP
                            | rights::INSPECT
                            | rights::TRANSFER,
                    )?
                    .into_handle(),
            );
            handles.push(
                self.ready
                    .duplicate(rights::SIGNAL | rights::TRANSFER)?
                    .into_handle(),
            );
            handles.push(
                self.progress
                    .duplicate(rights::SIGNAL | rights::WAIT | rights::TRANSFER)?
                    .into_handle(),
            );
            reply.u64(queue::MAGIC);
            state.negotiated = true;
            Ok(handles)
        })())
    }
    /// 消费至多一个容量的请求，合并完成通知；返回是否仍有请求供 dispatcher 公平重排。
    ///
    /// # Errors
    ///
    /// 损坏索引、重放 ticket 或对端 abort；调用者须退休连接。领域错误写入完成记录。
    pub fn drain(&self, socket: &ServedSocket) -> Result<bool, Status> {
        let mut state = self.state.lock();
        if self.ring.aborted() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let mut completed = false;
        for _ in 0..CAPACITY {
            let Some(request) = self.ring.request()? else {
                break;
            };
            let ticket = request[0];
            let index = ticket as usize & (CAPACITY - 1);
            if ticket == 0 || ticket <= state.last[index] {
                return Err(native_abi::ERR_BAD_STATE);
            }
            state.last[index] = ticket;
            let State {
                scratch, controls, ..
            } = &mut *state;
            let response = socket_io::execute(socket, &self.ring, request, scratch, controls)?;
            self.ring.complete(response)?;
            completed = true;
        }
        if completed {
            self.progress.signal(0, signals::SIGNALED)?;
        }
        self.ring.has_requests()
    }
    /// 公布终态并唤醒全部等待者；重复调用无副作用。
    pub fn close(&self) {
        self.ring.close();
        let _ = self.progress.signal(0, signals::SIGNALED);
    }
}
impl Drop for SocketQueue {
    fn drop(&mut self) {
        self.close();
    }
}
