//! seatd：文本控制台与桌面 compositor 之间的座位（显示器 + 键盘）仲裁进程。
//!
//! 只做三件事：接受 session 连接、把帧喂给 `seat_proto::Seat`、执行它返回的动作。切换规则全部在
//! 状态机里；这里只拥有 socket、超时和驱逐。seatd 不读 evdev：compositor 独占 grab 键盘，
//! 快捷键由持有座位的 session 自己识别后发 `Switch`。

use std::{
    collections::VecDeque,
    fs,
    io::{self, Read, Write},
    os::{
        fd::AsFd,
        unix::net::{UnixListener, UnixStream},
    },
    time::{Duration, Instant},
};

use linux_uapi::{
    process::{self, Pid, Signal},
    unix::{self, PollEvents, PollFd},
};
use seat_proto::{
    Action, ClientFrame, ClientId, Event, FRAME_BYTES, Role, SOCKET_PATH, Seat, ServerFrame,
};

/// 初始 session 在这段时间内没有注册，就回退到已注册的 session，避免显示器一直黑屏。
const STARTUP_GRACE: Duration = Duration::from_secs(5);
/// `Release` 发出后 session 必须在这段时间内回复；超时则终止其进程，由 init respawn。
const RELEASE_GRACE: Duration = Duration::from_secs(3);

/// 一条已接受的连接。`role` 为 `None` 表示尚未 Register（`seatctl` 永远不会注册）。
struct Connection {
    id: ClientId,
    stream: UnixStream,
    registered: bool,
    frame: [u8; FRAME_BYTES],
    filled: usize,
}

impl Connection {
    /// 读取一个完整帧；`Ok(None)` 表示数据还没到齐，`Err` 表示对端关闭或出错。
    fn read_frame(&mut self) -> io::Result<Option<[u8; FRAME_BYTES]>> {
        while self.filled < FRAME_BYTES {
            match self.stream.read(&mut self.frame[self.filled..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => self.filled += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.filled = 0;
        Ok(Some(self.frame))
    }
}

struct Daemon {
    seat: Seat,
    connections: Vec<Connection>,
    next_id: u64,
    /// `Release` 的回复期限；缺失该字段，无响应的 session 会让切换永久悬挂。
    release_deadline: Option<Instant>,
    announced: Option<Role>,
}

/// 运行时门禁依赖串口 marker：多个进程同时写 UART 时，分段写入会把一行拆开。整行一次 write 保证
/// marker 子串完整。
fn mark(line: String) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
}

fn main() {
    if let Err(error) = run() {
        eprintln!("seatd: {error}");
        std::process::exit(1);
    }
}

/// `/proc/cmdline` 里的 `liteos.target=text|desktop`；缺省或非法值回退到文本控制台。
fn initial_role() -> Role {
    let command_line = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    match command_line
        .split_whitespace()
        .find_map(|argument| argument.strip_prefix("liteos.target="))
    {
        None => Role::Text,
        Some(name) => Role::parse(name).unwrap_or_else(|| {
            eprintln!("seatd: unknown liteos.target={name}; using text");
            Role::Text
        }),
    }
}

fn role_name(role: Option<Role>) -> &'static str {
    match role {
        Some(Role::Text) => "text",
        Some(Role::Desktop) => "desktop",
        None => "none",
    }
}

fn run() -> io::Result<()> {
    let _ = fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH)?;
    listener.set_nonblocking(true)?;
    let initial = initial_role();
    mark(format!("seatd: ready target={}", role_name(Some(initial))));
    let mut daemon = Daemon {
        seat: Seat::new(initial),
        connections: Vec::new(),
        next_id: 1,
        release_deadline: None,
        announced: None,
    };
    let startup_deadline = Instant::now() + STARTUP_GRACE;
    let mut startup_pending = true;
    loop {
        let now = Instant::now();
        if startup_pending && now >= startup_deadline {
            startup_pending = false;
            daemon.dispatch(Event::StartupTimeout);
        }
        if daemon
            .release_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            daemon.release_deadline = None;
            daemon.dispatch(Event::ReleaseTimeout);
        }
        let mut deadline = daemon.release_deadline;
        if startup_pending {
            deadline = Some(deadline.map_or(startup_deadline, |d| d.min(startup_deadline)));
        }
        let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let mut descriptors = vec![PollFd::new(listener.as_fd(), PollEvents::READ)];
        descriptors.extend(
            daemon
                .connections
                .iter()
                .map(|connection| PollFd::new(connection.stream.as_fd(), PollEvents::READ)),
        );
        match unix::poll(&mut descriptors, timeout) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        let ready: Vec<ClientId> = daemon
            .connections
            .iter()
            .zip(&descriptors[1..])
            .filter(|(_, descriptor)| {
                descriptor
                    .returned()
                    .contains(PollEvents::READ | PollEvents::ERROR | PollEvents::HANGUP)
            })
            .map(|(connection, _)| connection.id)
            .collect();
        if descriptors[0].returned().contains(PollEvents::READ) {
            daemon.accept(&listener)?;
        }
        for id in ready {
            daemon.service(id);
        }
    }
}

impl Daemon {
    fn accept(&mut self, listener: &UnixListener) -> io::Result<()> {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    let id = ClientId(self.next_id);
                    self.next_id += 1;
                    self.connections.push(Connection {
                        id,
                        stream,
                        registered: false,
                        frame: [0; FRAME_BYTES],
                        filled: 0,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// 读取并处理一条连接上已到达的全部帧；出错或对端关闭即断开。
    fn service(&mut self, id: ClientId) {
        loop {
            let Some(index) = self.connections.iter().position(|c| c.id == id) else {
                return;
            };
            match self.connections[index].read_frame() {
                Ok(None) => return,
                Ok(Some(bytes)) => match ClientFrame::decode(bytes) {
                    Some(frame) => self.frame(id, frame),
                    None => return self.disconnect(id),
                },
                Err(_) => return self.disconnect(id),
            }
        }
    }

    fn frame(&mut self, id: ClientId, frame: ClientFrame) {
        match frame {
            ClientFrame::Register(role) => {
                if let Some(connection) = self.connection(id) {
                    connection.registered = true;
                }
                self.dispatch(Event::Registered(id, role));
            }
            ClientFrame::Released => {
                self.dispatch(Event::Released(id));
            }
            ClientFrame::Switch(role) => {
                let registered = self.connection(id).is_some_and(|c| c.registered);
                let result = self.dispatch(Event::Switch(role));
                // 只有未注册的请求者（seatctl）会收到结果；已注册 session 通过 Release/Acquire 得知。
                if !registered {
                    if let Some(result) = result {
                        self.reply(id, ServerFrame::Switched(result));
                    }
                    self.connections.retain(|c| c.id != id);
                }
            }
        }
    }

    fn connection(&mut self, id: ClientId) -> Option<&mut Connection> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    /// 回复未注册的请求者；失败说明对端已走，连接随后被丢弃，无需额外处理。
    fn reply(&mut self, id: ClientId, frame: ServerFrame) {
        if let Some(connection) = self.connection(id) {
            let _ = connection.stream.write_all(&frame.encode());
        }
    }

    fn disconnect(&mut self, id: ClientId) {
        if self.connections.iter().any(|c| c.id == id) {
            self.connections.retain(|c| c.id != id);
            self.dispatch(Event::Disconnected(id));
        }
    }

    /// 喂入事件并执行动作；`Evict`/发送失败产生的断开事件在同一个工作队列里继续处理。
    fn dispatch(&mut self, event: Event) -> Option<seat_proto::SwitchResult> {
        let mut queue = VecDeque::from([event]);
        let mut first_result = None;
        let mut first = true;
        while let Some(event) = queue.pop_front() {
            let was_pending = self.seat.release_pending();
            let (actions, result) = self.seat.handle(event);
            if first {
                first_result = result;
                first = false;
            }
            for action in actions {
                match action {
                    Action::Send(id, frame) => self.send_queued(id, frame, &mut queue),
                    Action::Evict(id) => self.evict(id, &mut queue),
                }
            }
            match (was_pending, self.seat.release_pending()) {
                (false, true) => self.release_deadline = Some(Instant::now() + RELEASE_GRACE),
                (_, false) => self.release_deadline = None,
                _ => {}
            }
        }
        if self.announced != self.seat.active() {
            self.announced = self.seat.active();
            mark(format!("seatd: active {}", role_name(self.announced)));
        }
        first_result
    }

    fn send_queued(&mut self, id: ClientId, frame: ServerFrame, queue: &mut VecDeque<Event>) {
        let failed = match self.connection(id) {
            Some(connection) => connection.stream.write_all(&frame.encode()).is_err(),
            None => return,
        };
        if failed {
            self.connections.retain(|c| c.id != id);
            queue.push_back(Event::Disconnected(id));
        }
    }

    /// 终止无响应 session 的进程并关闭连接；init 会 respawn 它，重新注册后由状态机重新分配座位。
    fn evict(&mut self, id: ClientId, queue: &mut VecDeque<Event>) {
        let pid = self
            .connections
            .iter()
            .find(|c| c.id == id)
            .and_then(|connection| unix::peer_credentials(connection.stream.as_fd()).ok())
            .and_then(|credentials| Pid::new(credentials.pid));
        if let Some(pid) = pid {
            eprintln!("seatd: evicting unresponsive session pid={}", pid.get());
            let _ = process::signal(pid, Signal::Kill);
        }
        self.connections.retain(|c| c.id != id);
        queue.push_back(Event::Disconnected(id));
    }
}
