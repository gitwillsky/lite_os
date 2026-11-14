//! 本机管理观察使用同一授权握手和消息封装，等待时交还应用执行器。

use super::*;
use router_protocol::firewall::counters::{Cursor, observation};
use router_protocol::neighbors;
use router_protocol::route_view;
use std::os::fd::BorrowedFd;

struct LocalObservation {
    session: Session,
}

impl LocalObservation {
    fn start_scoped(
        operation: u32,
        domain: Option<&str>,
        cursor: &[u8],
        timeout: Duration,
    ) -> io::Result<Self> {
        let control = administration::scoped_query(domain, cursor)
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        Self::start(operation, &control, timeout)
    }

    fn start(operation: u32, control: &[u8], timeout: Duration) -> io::Result<Self> {
        let mut session = local_session()?;
        session.get_mut().reset(timeout);
        send_request(&mut session, operation, control, None)?;
        let observation = Self { session };
        observation.stream().socket.set_nonblocking(true)?;
        Ok(observation)
    }

    fn stream(&self) -> &DeadlineStream<UnixStream> {
        let SessionIo::Unix(stream) = self.session.get_ref() else {
            unreachable!("local observation")
        };
        stream
    }

    fn remaining(&self) -> Duration {
        self.stream().remaining().unwrap_or_default()
    }

    fn try_receive(&mut self) -> io::Result<Option<tungstenite::Bytes>> {
        match receive(&mut self.session) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            result => result.map(Some),
        }
    }

    fn try_update<T>(
        &mut self,
        operation: u32,
        parse: impl FnOnce(&[u8]) -> Result<T, router_protocol::Invalid>,
    ) -> io::Result<Option<T>> {
        let Some(response) = self.try_receive()? else {
            return Ok(None);
        };
        let (body, length) = success_control(operation, &response)?;
        debug_assert_eq!(length, 0, "观察帧没有独立正文");
        let update = parse(body).map_err(|_| io::ErrorKind::InvalidData)?;
        self.session.get_mut().reset(RPC_TIMEOUT);
        Ok(Some(update))
    }
}

/// 已发出的本机连接变化等待。握手与发送在后台完成，结果可由事件循环等待。
pub struct PendingConnectionWatch(LocalObservation);

impl PendingConnectionWatch {
    /// 启动一次等待；调用可能阻塞，应在后台执行。释放句柄关闭本次会话。
    ///
    /// # Errors
    /// 本机网关不可用、对端身份不符、握手或发送失败。
    pub fn start(
        domain: Option<&str>,
        cursor: router_protocol::connections::changes::Cursor,
    ) -> io::Result<Self> {
        LocalObservation::start_scoped(
            ordinal::WAIT_CONNECTION_CHANGES,
            domain,
            &cursor.encode(),
            crate::settings::WATCH_TIMEOUT,
        )
        .map(Self)
    }

    /// 传输绝对期限的剩余时间，供外部就绪循环安排定时唤醒。
    pub fn remaining(&self) -> Duration {
        self.0.remaining()
    }

    /// 尝试接收等待结果。未收到完整回复时返回 false；成功后应释放本次会话并检查游标。
    ///
    /// # Errors
    /// 超时、连接关闭、无权限或协议回复无效。
    pub fn try_complete(&mut self) -> io::Result<bool> {
        let Some(response) = self.0.try_receive()? else {
            return Ok(false);
        };
        let (control, length) = success_control(ordinal::WAIT_CONNECTION_CHANGES, &response)?;
        if !control.is_empty() || length != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "转发连接等待回复无效",
            ));
        }
        Ok(true)
    }
}

impl AsFd for PendingConnectionWatch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.stream().socket.as_fd()
    }
}

/// 观察一页防火墙计数的本机流。释放句柄关闭会话，服务端停止采样。
pub struct FirewallObservation(LocalObservation);

impl FirewallObservation {
    /// 向本机管理网关订阅指定页。握手和发送可能阻塞，应在后台完成。
    /// # Errors
    /// 本机网关不可用、对端身份不符、握手或发送失败。
    pub fn start(domain: Option<&str>, cursor: Cursor) -> io::Result<Self> {
        LocalObservation::start_scoped(
            management::WATCH_FIREWALL_COUNTERS,
            domain,
            &cursor.encode(),
            RPC_TIMEOUT,
        )
        .map(Self)
    }

    /// 距离下一条观察结果或保活的超时期限，供事件循环安排定时唤醒。
    pub fn remaining(&self) -> Duration {
        self.0.remaining()
    }

    /// 非阻塞接收并解码下一条完整结果。返回 None 时等待描述符可读。
    /// # Errors
    /// 超时、会话关闭、无观察权限或正文无效。
    pub fn try_next(&mut self) -> io::Result<Option<observation::Update>> {
        self.0.try_update(
            management::WATCH_FIREWALL_COUNTERS,
            observation::Update::parse,
        )
    }
}

impl AsFd for FirewallObservation {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.stream().socket.as_fd()
    }
}

/// 本机邻居页观察流，释放时关闭本次订阅。
pub struct NeighborObservation {
    transport: LocalObservation,
    after: Option<neighbors::Key>,
}

impl NeighborObservation {
    /// 订阅所选游标后的一页邻居。握手和发送应在后台完成。
    /// # Errors
    /// 网关不可用、对端身份不符、握手或发送失败。
    pub fn start(domain: Option<&str>, after: Option<neighbors::Key>) -> io::Result<Self> {
        let transport = LocalObservation::start_scoped(
            management::WATCH_NEIGHBORS,
            domain,
            &neighbors::cursor(after),
            RPC_TIMEOUT,
        )?;
        Ok(Self { transport, after })
    }

    /// 下一帧的绝对期限剩余时间，供界面安排一次定时唤醒。
    pub fn remaining(&self) -> Duration {
        self.transport.remaining()
    }

    /// 非阻塞读取下一条已解码结果，返回 None 时等待可读。
    /// # Errors
    /// 会话关闭、权限不足、超时或回复格式无效。
    pub fn try_next(&mut self) -> io::Result<Option<neighbors::observation::Update>> {
        let after = self.after;
        self.transport
            .try_update(management::WATCH_NEIGHBORS, |body| {
                neighbors::observation::Update::parse(body, after)
            })
    }
}

impl AsFd for NeighborObservation {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.transport.stream().socket.as_fd()
    }
}

/// 单族生效路由页的本机观察流，释放时关闭订阅。
pub struct RouteObservation {
    transport: LocalObservation,
    cursor: route_view::Cursor,
}

impl RouteObservation {
    /// 订阅指定族与位置的路径页。握手和发送应在后台完成。
    /// # Errors
    /// 网关不可用、对端身份不符、握手或发送失败。
    pub fn start(domain: Option<&str>, cursor: route_view::Cursor) -> io::Result<Self> {
        let transport = LocalObservation::start_scoped(
            management::WATCH_ROUTES,
            domain,
            &cursor.encode(),
            RPC_TIMEOUT,
        )?;
        Ok(Self { transport, cursor })
    }

    /// 距下一帧截止的剩余时间。
    pub fn remaining(&self) -> Duration {
        self.transport.remaining()
    }

    /// 尝试读取完整观察结果，未收到时返回 None。
    /// # Errors
    /// 会话关闭、权限不足、超时或回复格式无效。
    pub fn try_next(&mut self) -> io::Result<Option<route_view::observation::Update>> {
        let cursor = self.cursor;
        self.transport.try_update(management::WATCH_ROUTES, |body| {
            route_view::observation::Update::parse(body, cursor)
        })
    }
}

impl AsFd for RouteObservation {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.transport.stream().socket.as_fd()
    }
}

/// 本机接口流量观察。释放句柄关闭本次订阅。
pub struct TrafficObservation {
    transport: LocalObservation,
    after: u32,
}

impl TrafficObservation {
    /// 订阅指定接口索引之后的一页，0 表示首页。应在后台握手。
    /// # Errors
    /// 网关不可用、身份不符、握手或发送失败。
    pub fn start(after: u32) -> io::Result<Self> {
        let transport = LocalObservation::start(
            management::WATCH_LINK_TRAFFIC,
            &management::traffic::request(after),
            RPC_TIMEOUT,
        )?;
        Ok(Self { transport, after })
    }

    /// 下一帧的绝对期限剩余时间。
    pub fn remaining(&self) -> Duration {
        self.transport.remaining()
    }

    /// 非阻塞读取已解码结果，未收到完整帧时返回 None。
    /// # Errors
    /// 会话关闭、权限不足、超时或回复无效。
    pub fn try_next(&mut self) -> io::Result<Option<management::traffic::Update>> {
        let after = self.after;
        self.transport
            .try_update(management::WATCH_LINK_TRAFFIC, |bytes| {
                management::traffic::Update::parse(bytes, after)
            })
    }
}

impl AsFd for TrafficObservation {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.transport.stream().socket.as_fd()
    }
}

/// 单接口的本地聚合流量订阅。关闭描述符即可停止。
pub struct FlowObservation(LocalObservation);

impl FlowObservation {
    /// 在后台握手并订阅本地接口。
    /// # Errors
    /// 网关不可用、身份不符、握手或发送失败。
    pub fn start(interface: u32) -> io::Result<Self> {
        LocalObservation::start(
            management::WATCH_FLOWS,
            &management::flows::request(interface),
            RPC_TIMEOUT,
        )
        .map(Self)
    }

    /// 传输期限剩余时间，供窗口安排超时唤醒。
    pub fn remaining(&self) -> Duration {
        self.0.remaining()
    }

    /// 非阻塞读取一个完整统计窗，未就绪时返回 None。
    /// # Errors
    /// 观察已关闭、权限不足、超时或回复无效。
    pub fn try_next(&mut self) -> io::Result<Option<management::flows::Sample>> {
        self.0
            .try_update(management::WATCH_FLOWS, management::flows::Sample::parse)
    }
}

impl AsFd for FlowObservation {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.stream().socket.as_fd()
    }
}
