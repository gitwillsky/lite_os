//! 设备目录：driver-host 持续服务的设备目录（对应 Fuchsia devfs）。
//!
//! 1. 驱动登记设备时只给出类、静态描述与“开会话”函数；目录分配稳定 ID，并通知所有观察者。
//! 2. 使用者经目录 channel `LIST` 枚举、`WATCH` 取得变化 Event、`OPEN` 建立新会话；会话句柄由设备类
//!    协议定义。会话结束（使用者关闭句柄、服务线程退出）时设备类回收会话状态，之后可重新打开。
//! 3. `SCOPE` 派生只含指定类的目录 channel：根服务据此把各类设备路由给不同服务，派生目录不能扩大范围。
//! 4. 独占类设备同一时刻只有一个会话。旧会话的使用者已离开时，`OPEN` 等待旧会话回收完毕再建立新会话；
//!    使用者仍在时返回 `ERR_ALREADY_BOUND`。
//! 5. 驱动宿主各自服务本进程设备的目录；driver-manager 把它们汇聚为一个目录（转发条目，
//!    [`Publisher::add`]），宿主终止时撤下其条目（[`Publisher::remove`]）。已撤下设备的会话
//!    随宿主一起 `PEER_CLOSED`；宿主重启后设备以新 ID 重新登记。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Handle};

use crate::{Reader, Writer, call, serve, take_handle};

const LIST: u32 = 1;
const WATCH: u32 = 2;
const OPEN: u32 = 3;
const SCOPE: u32 = 4;
/// 一次 `LIST` 回复的最多条目；使用者以最后一个 ID 续读。
const LIST_BATCH: usize = 32;

/// 设备类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Class {
    Block = 1,
    Network = 2,
    Display = 3,
    Input = 4,
    Pcm = 5,
    Port = 6,
    Entropy = 7,
    Usb = 8,
    /// 启动画面进度。
    Boot = 9,
}

impl Class {
    fn decode(value: u32) -> Result<Self, Status> {
        Ok(match value {
            1 => Self::Block,
            2 => Self::Network,
            3 => Self::Display,
            4 => Self::Input,
            5 => Self::Pcm,
            6 => Self::Port,
            7 => Self::Entropy,
            8 => Self::Usb,
            9 => Self::Boot,
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        })
    }

    /// 本类在 [`Classes`] 中的位。
    pub const fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// 类集合（[`Class::bit`] 的并）。
pub type Classes = u32;

/// 全部设备类。
pub const ALL_CLASSES: Classes = (1 << 10) - 2;

/// 目录中的一个设备。
pub struct Entry {
    /// 目录分配的稳定 ID（单调递增，不复用）。
    pub id: u64,
    pub class: Class,
    /// 类相关的静态描述。
    pub description: Vec<u8>,
}

impl Entry {
    /// 描述解码器。
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.description)
    }
}

/// 描述编码器（不含请求头）。
pub fn description() -> Writer {
    Writer::payload()
}

/// 会话结束标记：设备类把它放在会话状态的最后一个字段，状态回收后随之析构并置位 `SIGNALED`，
/// 使等待接管的 `OPEN` 继续。缺失时独占类的新会话可能与旧会话的回收并发，看到半回收的设备。
pub struct SessionEnd {
    ended: Event,
    watch: Option<Arc<Mutex<SessionWatch>>>,
}

enum SessionWatch {
    /// OPEN 尚未安装观测句柄；服务可能先于 OPEN 返回而结束。
    Opening,
    Active(Channel),
    /// 不得再次安装句柄保活死亡端点。
    Finished,
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        let retired = self
            .watch
            .as_ref()
            .map(|watch| core::mem::replace(&mut *watch.lock(), SessionWatch::Finished));
        // 目录观测不能延长已经停止的服务端寿命，否则未回复的 channel_call 永久等待。
        // 状态先于本结束标记析构；在锁外关闭最后的观测句柄，之后才通知可重新 OPEN。
        drop(retired);
        let _ = self.ended.signal(0, signals::SIGNALED);
    }
}

/// 会话的服务端状态：设备类自己的会话状态 `state` 在前、结束标记在后，析构顺序保证状态先回收。
/// 服务闭包经 [`SessionState::get`] 访问状态，从而整体捕获它（只引用字段会让闭包漏掉结束标记）。
pub struct SessionState<S> {
    state: S,
    _end: Option<SessionEnd>,
}

impl<S> SessionState<S> {
    pub fn new(state: S, end: SessionEnd) -> Self {
        Self {
            state,
            _end: Some(end),
        }
    }

    /// 会话派生的附属连接（如块设备子范围）：不是目录会话，没有结束标记。
    pub fn derived(state: S) -> Self {
        Self { state, _end: None }
    }

    pub fn get(&self) -> &S {
        &self.state
    }
}

/// 使用者端打开的设备：目录条目的类与描述，加上会话句柄。
pub struct OpenedDevice {
    pub class: Class,
    /// 类相关的静态描述。
    pub description: Vec<u8>,
    /// 设备类协议定义的会话句柄。
    pub handles: Vec<Handle>,
}

impl OpenedDevice {
    /// 描述解码器。
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.description)
    }
}

/// 设备类建立的会话：交给使用者的句柄与请求 channel 服务端的副本（用于判断使用者是否已离开）。
pub struct Session {
    pub handles: Vec<Handle>,
    pub server: Channel,
}

/// 设备类的开会话函数：解释本类 options、建立 channel；会话状态持有 `SessionEnd`。
pub type Opener = Box<dyn Fn(SessionEnd, &[u8]) -> Result<Session, Status> + Send + Sync>;

/// 转发条目的开会话函数：向设备所在宿主的目录发出 `OPEN`，原样返回会话句柄。
pub type ForwardOpener = Box<dyn Fn(&[u8]) -> Result<Vec<Handle>, Status> + Send + Sync>;

/// 服务会话请求 channel，并返回其服务端副本供目录判断会话存活。
///
/// # Errors
///
/// 句柄复制或服务线程创建失败。
pub fn serve_session(
    channel: Channel,
    workers: usize,
    name: &[u8],
    handler: Arc<service_rpc::Handler<'static>>,
) -> Result<Channel, Status> {
    let watch = channel.duplicate(rights::SAME)?;
    serve(channel, workers, name, handler)?;
    Ok(watch)
}

struct Live {
    // OWNER: 目录与 SessionEnd 共享同一观测 owner；结束标记先退休 server，再发表 ended。
    watch: Arc<Mutex<SessionWatch>>,
    ended: Event,
}

/// 会话从哪里建立。
enum Source {
    /// 本进程的驱动。
    Local {
        exclusive: bool,
        opener: Opener,
        // OWNER: 独占类的当前会话；缺失时两个使用者会同时驱动同一硬件。
        live: Mutex<Option<Live>>,
    },
    /// 另一宿主的目录：独占与会话回收由该宿主判定。
    Forwarded(ForwardOpener),
}

struct Device {
    id: u64,
    class: Class,
    description: Vec<u8>,
    source: Source,
}

impl Device {
    fn open(&self, options: &[u8]) -> Result<Vec<Handle>, Status> {
        let (exclusive, opener, live) = match &self.source {
            Source::Local {
                exclusive,
                opener,
                live,
            } => (*exclusive, opener, live),
            Source::Forwarded(open) => return open(options),
        };
        let mut live = live.lock();
        if exclusive && let Some(current) = live.as_ref() {
            // 使用者仍持有请求 channel：拒绝；已离开：等服务线程退出、会话状态回收完毕。
            {
                let watch = current.watch.lock();
                if let SessionWatch::Active(server) = &*watch {
                    match server.wait_one(signals::PEER_CLOSED, 0) {
                        Err(native_abi::ERR_TIMED_OUT) => {
                            return Err(native_abi::ERR_ALREADY_BOUND);
                        }
                        Err(status) => return Err(status),
                        Ok(_) => {}
                    }
                }
            }
            // 不能持观测锁等 ended：SessionEnd 的最后清理需要同一把锁。
            current
                .ended
                .wait_one(signals::SIGNALED, native_abi::DEADLINE_INFINITE)?;
        }
        let ended = Event::create()?;
        let watch = if exclusive {
            Some(
                Arc::try_new(Mutex::new(SessionWatch::Opening))
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?,
            )
        } else {
            None
        };
        let end = SessionEnd {
            ended: ended.duplicate(rights::SAME)?,
            watch: watch.clone(),
        };
        let session = opener(end, options)?;
        if let Some(watch) = watch {
            {
                let mut state = watch.lock();
                if matches!(*state, SessionWatch::Opening) {
                    *state = SessionWatch::Active(session.server);
                }
            }
            *live = Some(Live { watch, ended });
        }
        Ok(session.handles)
    }
}

struct State {
    devices: Vec<Arc<Device>>,
    // OWNER: 每个 `WATCH` 调用者的变化 Event；目录变化时全部置位。数量受使用者连接数约束。
    watchers: Vec<Event>,
    next: u64,
    enumerated: bool,
}

/// driver-host 端的设备目录。
pub struct Directory {
    state: Mutex<State>,
}

impl Directory {
    /// 创建空目录。
    ///
    /// # Errors
    ///
    /// 分配失败。
    pub fn new() -> Result<Arc<Self>, Status> {
        Arc::try_new(Self {
            state: Mutex::new(State {
                devices: Vec::new(),
                watchers: Vec::new(),
                next: 1,
                enumerated: false,
            }),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)
    }

    /// 登记本进程驱动的设备并通知观察者。
    ///
    /// # Returns
    ///
    /// 设备在本目录中的 ID。
    ///
    /// # Errors
    ///
    /// 分配失败。
    pub fn add(
        &self,
        class: Class,
        description: &Writer,
        exclusive: bool,
        opener: Opener,
    ) -> Result<u64, Status> {
        self.insert(
            class,
            description.as_bytes(),
            Source::Local {
                exclusive,
                opener,
                live: Mutex::new(None),
            },
        )
    }

    fn insert(&self, class: Class, description: &[u8], source: Source) -> Result<u64, Status> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(description.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.extend_from_slice(description);
        let mut state = self.state.lock();
        state
            .devices
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let id = state.next;
        let device = Arc::try_new(Device {
            id,
            class,
            description: bytes,
            source,
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        state.next += 1;
        state.devices.push(device);
        Self::notify(&state);
        Ok(id)
    }

    /// 服务独立的目录发布端。只有持有发布 capability 的驱动管理器可以修改目录。
    ///
    /// # Errors
    ///
    /// 无法启动发布服务线程。
    pub fn serve_publisher(self: &Arc<Self>, channel: Channel) -> Result<(), Status> {
        let directory = self.clone();
        serve(
            channel,
            1,
            b"device-publisher",
            Arc::new(move |ordinal, request, mut handles, reply| {
                match ordinal {
                    1 => {
                        let class = Class::decode(request.u32()?)?;
                        let id = request.u64()?;
                        let description = request.bytes()?;
                        let source = DirectoryClient::new(Channel::from_handle(take_handle(
                            &mut handles,
                            0,
                        )?));
                        let open = Box::new(move |options: &[u8]| source.open_handles(id, options));
                        reply.u64(directory.insert(class, description, Source::Forwarded(open))?);
                    }
                    2 => directory.remove(request.u64()?),
                    3 => directory.enumerated(),
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            }),
        )
    }

    /// 撤下设备并通知观察者；已建立的会话不受目录影响。
    pub fn remove(&self, id: u64) {
        let removed = {
            let mut state = self.state.lock();
            let removed = state
                .devices
                .iter()
                .position(|device| device.id == id)
                .map(|index| state.devices.remove(index));
            if removed.is_some() {
                Self::notify(&state);
            }
            removed
        };
        // 开会话闭包可能持有宿主连接，在目录锁外释放。
        drop(removed);
    }

    /// 启动时发现的设备已全部登记。
    pub fn enumerated(&self) {
        let mut state = self.state.lock();
        state.enumerated = true;
        Self::notify(&state);
    }

    fn notify(state: &State) {
        for watcher in &state.watchers {
            let _ = watcher.signal(0, signals::USER_0);
        }
    }

    fn device(&self, id: u64, scope: Classes) -> Result<Arc<Device>, Status> {
        self.state
            .lock()
            .devices
            .iter()
            .find(|device| device.id == id && scope & device.class.bit() != 0)
            .cloned()
            .ok_or(native_abi::ERR_NOT_FOUND)
    }

    /// 以 `scope` 范围服务一条目录 channel。
    ///
    /// # Errors
    ///
    /// 服务线程或闭包分配失败。
    pub fn serve(self: &Arc<Self>, channel: Channel, scope: Classes) -> Result<(), Status> {
        let directory = self.clone();
        serve(
            channel,
            1,
            b"device-directory",
            Arc::try_new(
                move |ordinal: u32,
                      request: &mut Reader<'_>,
                      _: Vec<Handle>,
                      reply: &mut Writer| {
                    directory.handle(scope, ordinal, request, reply)
                },
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?,
        )
    }

    fn handle(
        self: &Arc<Self>,
        scope: Classes,
        ordinal: u32,
        request: &mut Reader<'_>,
        reply: &mut Writer,
    ) -> Result<Vec<Handle>, Status> {
        match ordinal {
            LIST => {
                let after = request.u64()?;
                let state = self.state.lock();
                let entries: Vec<_> = state
                    .devices
                    .iter()
                    .filter(|device| device.id > after && scope & device.class.bit() != 0)
                    .take(LIST_BATCH)
                    .collect();
                reply.bool(state.enumerated).u32(entries.len() as u32);
                for device in entries {
                    reply
                        .u64(device.id)
                        .u32(device.class as u32)
                        .bytes(&device.description);
                }
                Ok(Vec::new())
            }
            WATCH => {
                let event = Event::create()?;
                let peer = event.duplicate(rights::SAME)?;
                let mut state = self.state.lock();
                state
                    .watchers
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                state.watchers.push(event);
                Ok(alloc::vec![peer.into_handle()])
            }
            OPEN => {
                let device = self.device(request.u64()?, scope)?;
                device.open(request.bytes()?)
            }
            SCOPE => {
                let narrowed = request.u32()?;
                if narrowed & !scope != 0 {
                    return Err(native_abi::ERR_ACCESS_DENIED);
                }
                let (client, server) = Channel::create()?;
                self.serve(server, narrowed)?;
                Ok(alloc::vec![client.into_handle()])
            }
            _ => Err(native_abi::ERR_NOT_SUPPORTED),
        }
    }
}

/// 使用者端的目录连接。
pub struct DirectoryClient(Channel);

impl DirectoryClient {
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }

    /// 读取 ID 大于 `after` 的下一批设备；返回（启动枚举是否已完成，设备）。批为空表示暂无更多。
    ///
    /// # Errors
    ///
    /// 目录已关闭或回复格式错误。
    pub fn list(&self, after: u64) -> Result<(bool, Vec<Entry>), Status> {
        let mut request = Writer::request(LIST);
        request.u64(after);
        let reply = call(&self.0, &request, Vec::new())?;
        let mut reader = reply.reader();
        let enumerated = reader.bool()?;
        let count = reader.u32()? as usize;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count.min(LIST_BATCH))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for _ in 0..count.min(LIST_BATCH) {
            let id = reader.u64()?;
            let class = Class::decode(reader.u32()?)?;
            let description = reader.bytes()?.to_vec();
            entries.push(Entry {
                id,
                class,
                description,
            });
        }
        Ok((enumerated, entries))
    }

    /// 取得本连接的变化 Event：目录增加设备或启动枚举完成时置位 `USER_0`，使用者清除后再 `list`。
    ///
    /// # Errors
    ///
    /// 目录已关闭。
    pub fn watch(&self) -> Result<Event, Status> {
        let mut reply = call(&self.0, &Writer::request(WATCH), Vec::new())?;
        Ok(Event::from_handle(take_handle(&mut reply.handles, 0)?))
    }

    /// 按设备类的版本化 options 建立会话；目录原样转发选项，不解释设备配置。
    ///
    /// # Errors
    ///
    /// `ERR_NOT_FOUND`（不在本目录范围）、`ERR_ALREADY_BOUND`（独占类已有使用者）或设备类建立失败。
    pub fn open(&self, entry: &Entry, options: &[u8]) -> Result<OpenedDevice, Status> {
        let handles = self.open_handles(entry.id, options)?;
        let mut description = Vec::new();
        description
            .try_reserve_exact(entry.description.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        description.extend_from_slice(&entry.description);
        Ok(OpenedDevice {
            class: entry.class,
            description,
            handles,
        })
    }

    fn open_handles(&self, id: u64, options: &[u8]) -> Result<Vec<Handle>, Status> {
        let mut request = Writer::request(OPEN);
        request.u64(id).bytes(options);
        Ok(call(&self.0, &request, Vec::new())?.handles)
    }

    /// 复制底层目录能力，保留同一权限及会话。
    ///
    /// # Errors
    ///
    /// 内核无法复制句柄。
    pub fn duplicate(&self) -> Result<Channel, Status> {
        self.0.duplicate(rights::SAME)
    }

    /// 派生只含 `classes` 的目录连接。
    ///
    /// # Errors
    ///
    /// `ERR_ACCESS_DENIED`（超出本目录范围）或目录已关闭。
    pub fn scope(&self, classes: Classes) -> Result<Self, Status> {
        let mut request = Writer::request(SCOPE);
        request.u32(classes);
        let mut reply = call(&self.0, &request, Vec::new())?;
        Ok(Self(Channel::from_handle(take_handle(
            &mut reply.handles,
            0,
        )?)))
    }

    /// 交出底层 channel（转交给另一个服务）。
    pub fn into_channel(self) -> Channel {
        self.0
    }
}

/// 汇聚目录的发布能力。目录及已建立的使用者连接由根服务持有。
pub struct Publisher(Channel);

impl Publisher {
    /// 接管发布 channel。
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }

    /// 发布宿主目录条目，返回汇聚目录分配的 ID。
    ///
    /// # Errors
    ///
    /// 宿主目录能力复制、请求或目录登记失败。
    pub fn add(&self, source: &DirectoryClient, entry: &Entry) -> Result<u64, Status> {
        let mut request = Writer::request(1);
        request
            .u32(entry.class as u32)
            .u64(entry.id)
            .bytes(&entry.description);
        call(
            &self.0,
            &request,
            alloc::vec![source.duplicate()?.into_handle()],
        )?
        .reader()
        .u64()
    }

    /// 撤下汇聚条目。已建立的设备会话由宿主维持。
    ///
    /// # Errors
    ///
    /// 发布端已关闭或无法回复。
    pub fn remove(&self, id: u64) -> Result<(), Status> {
        let mut request = Writer::request(2);
        request.u64(id);
        call(&self.0, &request, Vec::new()).map(drop)
    }

    /// 标记引导枚举完成。
    ///
    /// # Errors
    ///
    /// 发布端已关闭或无法回复。
    pub fn enumerated(&self) -> Result<(), Status> {
        call(&self.0, &Writer::request(3), Vec::new()).map(drop)
    }
}
