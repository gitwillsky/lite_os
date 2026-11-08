//! 显示会话主循环：输入、Wayland 连接、窗口栈、桌面外壳与整帧合成的唯一装配点。

mod cursor_requests;
mod hit_testing;
mod presentation;
mod seat_input;
mod window_menu_commands;
mod window_requests;
mod window_switching;
use std::{
    collections::{BTreeSet, HashMap},
    io,
    os::{fd::AsFd, unix::net::UnixStream},
    path::Path,
    process::Child,
    time::{Duration, Instant},
};

use classic_theme::Theme;

use crate::{
    switcher::{self, Switcher},
    window_menu::{self, WindowMenu},
};
use linux_uapi::{
    drm::DumbBuffer,
    process::spawn_with_wayland_socket,
    unix::{PollBuffer, PollEvents, PollFd},
};
use wayland_wire::wire::ObjectId;

use crate::{
    boot::{self, Progress},
    compose::Drawable,
    cursor::{self, CursorSet},
    desktop::{Geometry, Initial, Mode, Stack},
    focus::{Hit, Seat, Shortcut, local_position, window_configure},
    input::{self, Input},
    interaction::{Edges, Grab, Kind},
    scanout::Scanout,
    shell_layout,
    wayland::{
        ClientId, ForeignAction, ForeignInfo, ForeignState, KeyboardInteractivity, Layer, Server,
        WindowCommand, WindowRequest,
    },
};

/// Linux `BTN_LEFT`（`linux/input-event-codes.h`）。
const BUTTON_LEFT: u16 = 0x110;
const BUTTON_RIGHT: u16 = 0x111;
/// 键盘移动/缩放窗口时方向键每次的步长，逻辑像素。
const KEYBOARD_STEP: i32 = 8;

/// 最小化/还原的整窗几何变换；只标识现有已提交 surface，不光栅第二份标题栏或复制客户区。
struct Animation {
    client: ClientId,
    surface: ObjectId,
    generation: u64,
    from: (i32, i32, i32, i32),
    to: (i32, i32, i32, i32),
    start: Instant,
}

/// 动画时长。
const ANIMATION: Duration = Duration::from_millis(150);
// 连续视频/缩放复用暂存；停止上传一秒且 fence 完成后回收峰值，否则空闲桌面会保留整屏副本。
// 期限只加入已有 poll，缓存为空时不增加唤醒；策略参照 Skia 的 deferred scratch cleanup。
const UPLOAD_IDLE: Duration = Duration::from_secs(1);

impl Animation {
    fn finished(&self) -> bool {
        self.start.elapsed() >= ANIMATION
    }

    /// 当前帧的矩形：时间进度以 smoothstep 缓入缓出，同一帧所有 surface 使用同一个插值结果。
    fn frame(&self) -> (i32, i32, i32, i32) {
        let t = (self.start.elapsed().as_secs_f32() / ANIMATION.as_secs_f32()).clamp(0.0, 1.0);
        let t = t * t * (3.0 - 2.0 * t);
        let mix = |a: i32, b: i32| a + ((i64::from(b) - i64::from(a)) as f32 * t).round() as i32;
        (
            mix(self.from.0, self.to.0),
            mix(self.from.1, self.to.1),
            mix(self.from.2, self.to.2),
            mix(self.from.3, self.to.3),
        )
    }
}

/// 进行中的指针抓取，以及最近一次发给客户端的缩放目标（尺寸没变就不重复发 configure）。
struct ActiveGrab {
    grab: Grab,
    last_size: Option<(i32, i32)>,
    /// 系统菜单"移动/大小"发起的抓取：不依赖按住按钮，由点击或 Enter 结束、Esc 取消，方向键微调。
    /// 缺少它，从菜单选"移动"后一松手就结束，菜单命令等于无效。
    sticky: bool,
    /// 抓取开始时的窗口几何，Esc 取消时恢复。
    origin: (i32, i32, i32, i32),
}

/// 一个已定位的外壳 surface（物理坐标）。
#[derive(Clone, Copy)]
struct LayerBox {
    client: ClientId,
    surface: ObjectId,
    layer: Layer,
    keyboard: KeyboardInteractivity,
    x: i32,
    y: i32,
}

/// 可信来源仅提升安全覆盖层；背景必须让之后加入的用户桌面正常覆盖。
fn layer_order(layer: Layer, trusted: bool) -> (Layer, bool) {
    (layer, layer == Layer::Overlay && trusted)
}

/// 光标当前的形状来源。
#[derive(Clone, Copy)]
enum Shape {
    Default,
    Hidden,
    /// 合成器自己的光标（缩放抓取时的方向箭头）。
    Named(&'static str),
    /// 客户端用 `wl_pointer.set_cursor` 指定的图像。
    Client {
        client: ClientId,
        surface: ObjectId,
        hotspot: (i32, i32),
        offset_origin: (i64, i64),
    },
}

/// 启动画面阶段的状态：画面数据、进度、动画带的起始时刻，以及上一次计时与最后一次画出的帧。
struct BootScreen {
    scene: boot::Scene,
    progress: Progress,
    /// 动画带从交接时刻起转动；平移量只由经过的时间决定，丢帧不会让动画变慢。
    band_start: Instant,
    last_tick: Instant,
    painted: boot::Frame,
}

/// 建立会话所需的全部已就绪部件。
pub struct Parts {
    pub scanout: Scanout,
    pub input: Input,
    pub server: Server,
    pub theme: Theme,
    pub scale: u32,
    pub cursors: CursorSet,
    pub hardware_cursor: DumbBuffer,
    /// 已校验的 `LBS2` 启动画面。
    pub boot_scene: boot::Scene,
}

/// 一次隐式抓取的唯一事实：实际接收按下事件的 leaf、serial 与事件时的位置。
/// Session 的按键集合负责寿命；分发延迟不能把锚点替换为后续指针位置。
#[derive(Clone, Copy)]
struct PointerGrab {
    target: (ClientId, ObjectId),
    generation: Option<u64>,
    serial: u32,
    position: (i32, i32),
}

pub struct Session {
    scanout: Scanout,
    input: Input,
    server: Server,
    theme: Theme,
    /// 输出的整数 scale。
    scale: u32,
    cursors: CursorSet,
    /// 硬件光标的图像缓冲；释放它会撤掉光标图像，所以要在整个会话期间持有。
    hardware_cursor: DumbBuffer,
    shape: Shape,
    /// 成功上传的客户端光标像素与热点；不可变引用避免场景 dirty 时反复上传同一图像。
    applied_client_cursor: Option<(crate::wayland::Content, (u32, u32))>,
    last_pointer_focus: Option<((ClientId, ObjectId), u32)>,
    /// 当前按下的指针按钮。非空期间指针焦点固定在按下时的 surface（Wayland 的隐式抓取）：
    /// 否则拖动 layer surface 这类“随指针移动”的界面时，指针一旦跑出 surface 就收不到后续 motion 与释放。
    pressed_buttons: BTreeSet<u16>,
    /// 隐式抓取；所有按钮释放或目标消失时清除，不在 Seat 复制按钮 serial。
    pointer_grab: Option<PointerGrab>,
    /// 点击选中的 OnDemand 外壳；与 seat 的实际焦点分开保存策略，在窗口点击或图层消失时撤销。
    keyboard_layer: Option<(ClientId, ObjectId)>,
    /// 合成器拉起的外壳进程（`logon`）；目前只检测它的退出。
    shell: Option<Child>,
    /// compositor 创建的初始连接；仅它拥有 SAS 路由，委派 Shell 不能替代。
    logon_client: Option<ClientId>,
    /// SAS 后直到可信 Overlay 映射的输入屏障；缺少它会把握手期间的输入泄露给旧窗口。
    secure_pending: bool,
    /// 只有本地键盘更新 SAS 的修饰键；共享 seat 的远程修饰键不能参与安全注意序列。
    sas_modifiers: keyboard::Modifiers,
    stack: Stack,
    /// 按住 Alt 期间的切换面板；松开 Alt 才激活选中窗口，缺少它 Alt+Tab 每按一次就改一次栈序，看不到目标。
    switcher: Option<Switcher>,
    /// 面板的字体与图标，首次打开时加载；加载失败则只切换不显示面板。
    switcher_art: Option<switcher::Art>,
    /// 窗口（系统）菜单；打开期间独占指针与键盘，缺少它菜单外的点击会落到窗口上。
    window_menu: Option<WindowMenu>,
    /// 上一次宣告剪贴板选择的键盘焦点客户端；变化时重新宣告，缺少它新聚焦的程序看不到已复制的内容。
    selection_focus: Option<ClientId>,
    /// 外壳用 `set_rectangle` 告知的任务按钮位置（输出物理坐标），最小化/还原动画的目标。
    minimize_targets: HashMap<(ClientId, ObjectId, u64), Geometry>,
    /// 进行中的最小化/还原动画。
    animation: Option<Animation>,
    seat: Seat,
    grab: Option<ActiveGrab>,
    /// 第一个窗口或外壳 surface 出现之前一直显示启动画面；缺少它，空桌面会盖掉内核交接的画面。
    boot: Option<BootScreen>,
    dirty: bool,
    commits_seen: u64,
    /// 已映射的外壳 surface 数；变化时打印一行，运行时门禁据此确认外壳已上屏。
    layers_seen: usize,
    /// 一次性标记：运行时门禁据此确认输入路径端到端可用；缺少它，门禁只能靠截图间接判断指针是否被读到。
    saw_pointer: bool,
    saw_key: bool,
    started: Instant,
}

impl Session {
    /// 建立会话并显示初始光标。
    ///
    /// # Errors
    ///
    /// 缺少 `arrow` 光标，或硬件光标更新失败。
    pub fn new(mut parts: Parts) -> io::Result<Self> {
        parts.server.set_gpu_context(parts.scanout.gpu_context());
        let mut session = Self {
            scanout: parts.scanout,
            input: parts.input,
            server: parts.server,
            theme: parts.theme,
            scale: parts.scale,
            cursors: parts.cursors,
            hardware_cursor: parts.hardware_cursor,
            shape: Shape::Default,
            applied_client_cursor: None,
            last_pointer_focus: None,
            keyboard_layer: None,
            shell: None,
            logon_client: None,
            secure_pending: false,
            sas_modifiers: keyboard::Modifiers::default(),
            stack: Stack::default(),
            switcher: None,
            switcher_art: None,
            window_menu: None,
            selection_focus: None,
            minimize_targets: HashMap::new(),
            animation: None,
            seat: Seat::default(),
            grab: None,
            pressed_buttons: BTreeSet::new(),
            pointer_grab: None,
            boot: Some(BootScreen {
                scene: parts.boot_scene,
                progress: Progress::new(boot::HANDOFF_PERMILLE),
                band_start: Instant::now(),
                last_tick: Instant::now(),
                painted: boot::Frame {
                    offset: 0,
                    permille: boot::HANDOFF_PERMILLE,
                },
            }),
            dirty: false,
            commits_seen: 0,
            layers_seen: 0,
            saw_pointer: false,
            saw_key: false,
            started: Instant::now(),
        };
        session.apply_cursor()?;
        Ok(session)
    }

    /// 拉起外壳程序（`logon`）：它经 `WAYLAND_SOCKET` 得到一条特权连接，能绑定 layer-shell 与 foreign-toplevel。
    ///
    /// # Errors
    ///
    /// 创建 socketpair 或启动进程失败。
    pub fn spawn_shell(&mut self, program: &Path) -> io::Result<()> {
        let (server_end, client_end) = UnixStream::pair()?;
        self.logon_client = Some(self.server.add_session(server_end)?);
        self.shell = Some(spawn_with_wayland_socket(
            program.as_os_str(),
            client_end.into(),
            None,
        )?);
        println!("compositor: shell started {}", program.display());
        Ok(())
    }

    /// 真实的启动里程碑达成：进度条的目标提高到 `permille`（不会降低）。
    pub fn boot_milestone(&mut self, permille: u16) {
        if let Some(boot) = &mut self.boot
            && boot.progress.raise_target(permille)
        {
            println!("compositor: boot milestone {permille}");
        }
    }

    /// 启动画面阶段每个循环做一次：推进显示值、重绘进度条，走满且外壳有内容时结束这一阶段。
    ///
    /// # Returns
    ///
    /// 仍在追赶目标时返回下一次需要的唤醒间隔（用作 poll 超时）。
    fn step_boot(&mut self, shell_visible: bool) -> io::Result<Option<Duration>> {
        let Some(boot) = &mut self.boot else {
            return Ok(None);
        };
        let now = Instant::now();
        let elapsed = now.duration_since(std::mem::replace(&mut boot.last_tick, now));
        boot.progress.advance(elapsed.as_secs_f32());
        let since = now.duration_since(boot.band_start).as_millis() as u64;
        let frame = boot::Frame {
            offset: boot_scene::band_offset(since),
            permille: boot.progress.permille(),
        };
        let flip_busy = self.scanout.flip_pending();
        if frame != boot.painted && !flip_busy {
            self.scanout.paint_boot(&boot.scene, frame)?;
            boot.painted = frame;
            self.scanout.request_flip()?;
        } else if boot.progress.complete()
            && boot.painted.permille == 1000
            && !flip_busy
            && shell_visible
        {
            // 外壳已经有内容、进度条也走满并已上屏：交给桌面合成。
            self.boot = None;
            self.dirty = true;
            println!("compositor: boot complete");
            return Ok(None);
        }
        // 动画带一直在转：下一次唤醒取进度追赶的 16 毫秒与下一次平移时刻中较早者。
        let next_step = boot_scene::BAND_STEP_MS - since % boot_scene::BAND_STEP_MS;
        let wake = Duration::from_millis(next_step);
        Ok(Some(if boot.progress.animating() {
            wake.min(Duration::from_millis(16))
        } else {
            wake
        }))
    }

    pub fn input_devices(&self) -> usize {
        self.input.devices().count()
    }

    fn now_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn screen(&self) -> (i32, i32) {
        (self.scanout.width() as i32, self.scanout.height() as i32)
    }

    fn full_area(&self) -> Geometry {
        let (width, height) = self.screen();
        (0, 0, width, height)
    }

    /// 已映射的外壳 surface 的位置，按层级从底到顶排序。
    fn layer_boxes(&self) -> Vec<LayerBox> {
        let screen = self.screen();
        let scale = self.scale as i32;
        let work_area = self.work_area();
        let mut boxes: Vec<LayerBox> = self
            .server
            .layers()
            .map(|(client, layer)| {
                let size = layer.content.output_size(scale);
                // layer-shell：独占区为 0 的 surface 避开其他 surface 的独占区，其余相对整个输出。
                let area = if layer.state.exclusive_zone == 0 {
                    work_area
                } else {
                    (0, 0, screen.0, screen.1)
                };
                let (x, y) = shell_layout::place(layer.state, size, area, scale);
                LayerBox {
                    client,
                    surface: layer.surface,
                    layer: layer.state.layer,
                    keyboard: layer.state.keyboard,
                    x,
                    y,
                }
            })
            .collect();
        boxes
            .sort_by_key(|layer| layer_order(layer.layer, Some(layer.client) == self.logon_client));
        boxes
    }

    /// 工作区：整个输出扣除外壳的独占区。
    fn work_area(&self) -> Geometry {
        shell_layout::work_area(
            self.server.layers().map(|(_, layer)| layer.state),
            self.screen(),
            self.scale as i32,
        )
    }

    /// 独占键盘的外壳 surface（登录屏）：只有 Top 与 Overlay 层有效，层级最高者优先。
    fn exclusive_keyboard(&self, boxes: &[LayerBox]) -> Option<(ClientId, ObjectId)> {
        boxes
            .iter()
            .filter(|layer| {
                layer.keyboard == KeyboardInteractivity::Exclusive && layer.layer >= Layer::Top
            })
            .max_by_key(|layer| layer_order(layer.layer, Some(layer.client) == self.logon_client))
            .map(|layer| (layer.client, layer.surface))
    }

    /// 全屏组只在普通窗口持有键盘时压住 Top 外壳；绘制和命中共用此边界。
    /// 按需外壳取得键盘（例如运行对话框或任务按钮导航）时，不能仍被后台全屏窗口遮住。
    fn fullscreen_start(&self) -> Option<usize> {
        self.keyboard_layer
            .is_none()
            .then(|| self.stack.fullscreen_start())
            .flatten()
    }

    /// 普通桌面的按需键盘目标；安全登录图层不参与桌面焦点回退。
    fn desktop_keyboard(&self, boxes: &[LayerBox]) -> Option<(ClientId, ObjectId)> {
        boxes
            .iter()
            .find(|layer| {
                Some(layer.client) != self.logon_client
                    && layer.layer == Layer::Background
                    && layer.keyboard == KeyboardInteractivity::OnDemand
            })
            .map(|layer| (layer.client, layer.surface))
    }

    /// 运行到出错为止；正常情况下不返回。
    ///
    /// # Errors
    ///
    /// DRM、输入或监听 socket 的不可恢复错误。
    pub fn run(mut self) -> io::Result<()> {
        let runtime =
            std::env::var_os("XDG_RUNTIME_DIR").expect("runtime directory checked at startup");
        let mut remote = match crate::rfb::Desktop::bind(Path::new(&runtime)) {
            Ok(remote) => remote,
            Err(error) => {
                eprintln!("compositor: RFB unavailable: {error}");
                None
            }
        };
        let mut events = Vec::new();
        // poll slot 和结果保留容量；每轮重建借用，输入设备/连接改变后不会轮询已关闭的编号。
        let mut poll_buffer = PollBuffer::default();
        let mut returned = Vec::new();
        loop {
            let shell_visible =
                !self.stack.windows().is_empty() || self.server.layers().next().is_some();
            if shell_visible {
                // 外壳第一个 surface 出现：这是启动完成的真实信号。
                self.boot_milestone(1000);
            }
            let timeout = self.step_boot(shell_visible)?;
            if let Some(animation) = &self.animation {
                if animation.finished() {
                    self.animation = None;
                }
                // 每次 DRM flip 完成后合成下一帧，不再叠加 16 ms 定时等待。
                self.dirty = true;
            }
            if self.dirty && self.boot.is_none() && !self.scanout.flip_pending() {
                self.present()?;
                self.dirty = false;
            }
            let upload_wait = self
                .scanout
                .gpu_context()
                .map(|gpu| gpu.expire_upload(UPLOAD_IDLE))
                .transpose()?
                .flatten();
            let timeout = timeout.into_iter().chain(upload_wait).min();
            let input_count = self.input.devices().count();
            {
                let mut descriptors = poll_buffer.prepare();
                descriptors.push(PollFd::new(self.scanout.device().as_fd(), PollEvents::READ));
                descriptors.extend(
                    self.input
                        .devices()
                        .map(|device| PollFd::new(device.file().as_fd(), PollEvents::READ)),
                );
                // 输入热插拔的 uevent 订阅；没有 netlink 时用一个永不就绪的占位，保持下标布局固定。
                descriptors.push(PollFd::new(
                    self.input
                        .uevent_fd()
                        .unwrap_or_else(|| self.scanout.device().as_fd()),
                    if self.input.uevent_fd().is_some() {
                        PollEvents::READ
                    } else {
                        PollEvents::EMPTY
                    },
                ));
                descriptors.extend(self.server.descriptors());
                if let Some(remote) = &remote {
                    descriptors.push(remote.descriptor());
                }
                match descriptors.wait(timeout) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                    Ok(_) => {}
                }
                returned.clear();
                returned.extend(descriptors.sources().iter().map(PollFd::returned));
            }
            let remote_ready = remote.as_ref().and_then(|_| returned.pop());
            let drm_ready = returned[0].contains(PollEvents::READ);
            if drm_ready && self.scanout.flip_pending() {
                self.scanout.complete_flip()?;
                let time = self.now_ms();
                self.server.finish_frame(time);
            }
            for index in 0..input_count {
                let returned = returned[1 + index];
                if returned.contains(PollEvents::READ) {
                    self.input.drain(index, &mut events)?;
                } else if returned.contains(PollEvents::HANGUP | PollEvents::ERROR) {
                    self.input.forget(index);
                }
            }
            self.input.prune();
            if returned[1 + input_count].contains(PollEvents::READ) {
                self.input.handle_uevents()?;
            }
            // 已由前一轮按钮按下授权的 start_drag/accept 先于同轮硬件释放生效。
            self.server.dispatch(&returned[2 + input_count..])?;
            // 先退休旧映射及其授权，再处理同轮输入；否则旧抓取可能命中新一代窗口。
            let windows_changed = self.sync_windows()?;
            self.sync_work_area();
            if windows_changed {
                self.sync_focus();
            }
            self.process_data_device();
            // 一批积压的位置只应用最终状态，避免逐样本提交光标与 resize configure。
            input::coalesce_motion(&mut events);
            for event in events.drain(..) {
                self.on_input(event, true)?;
            }
            if let (Some(remote), Some(ready)) = (&mut remote, remote_ready) {
                remote.dispatch(ready, &self.scanout, &mut events);
                for event in events.drain(..) {
                    if let input::Event::PointerMoved { x, y } = event {
                        self.input.set_position(x, y);
                    }
                    self.on_input(event, false)?;
                }
            }
            self.handle_requests()?;
            self.sync_shell();
            self.sync_selection();
            self.sync_foreign();
            self.sync_focus();
            self.handle_cursor_requests()?;
            self.check_shell();
        }
    }

    /// 窗口菜单或键盘移动/缩放期间的按键：全部消费，返回是否已处理。
    fn modal_key(&mut self, code: u16, pressed: bool) -> io::Result<bool> {
        let sticky = self.grab.as_ref().is_some_and(|active| active.sticky);
        if self.window_menu.is_none() && !sticky {
            return Ok(false);
        }
        self.seat.swallow_key(code, pressed);
        if !pressed {
            return Ok(true);
        }
        let code = u32::from(code);
        if let Some(menu) = &mut self.window_menu {
            let (client, surface) = (menu.client, menu.surface);
            match menu.key(code) {
                None => self.dirty = true,
                Some(command) => {
                    self.close_menu();
                    if let Some(command) = command {
                        self.run_menu_command(client, surface, command)?;
                    }
                }
            }
            return Ok(true);
        }
        let step = KEYBOARD_STEP * self.scale as i32;
        let (dx, dy) = match code {
            105 => (-step, 0),
            106 => (step, 0),
            103 => (0, -step),
            108 => (0, step),
            _ => (0, 0),
        };
        match code {
            28 => self.end_grab()?,
            1 => self.cancel_grab()?,
            _ if (dx, dy) != (0, 0) => {
                if let Some(active) = &mut self.grab {
                    // 位移累加到起点上，之后鼠标移动继续以它为基准。
                    let start = &mut active.grab.start;
                    match active.grab.kind {
                        Kind::Move => (start.0, start.1) = (start.0 + dx, start.1 + dy),
                        Kind::Resize(_) => (start.2, start.3) = (start.2 + dx, start.3 + dy),
                    }
                }
                let pointer = self.input.pointer().position();
                self.drag(pointer);
            }
            _ => {}
        }
        Ok(true)
    }

    /// Esc：把键盘移动/缩放的窗口恢复到抓取之前的几何后结束。
    fn cancel_grab(&mut self) -> io::Result<()> {
        let Some(active) = &self.grab else {
            return Ok(());
        };
        let (grab, origin) = (active.grab, active.origin);
        self.dirty |= self
            .stack
            .move_to(grab.client, grab.surface, origin.0, origin.1);
        if matches!(grab.kind, Kind::Resize(_)) {
            let size = (origin.2, origin.3);
            self.send_configure(grab.client, grab.surface, Some(size), true);
            if let Some(active) = &mut self.grab {
                active.last_size = Some(size);
            }
        }
        self.end_grab()
    }

    fn on_shortcut(&mut self, shortcut: Shortcut) {
        if shortcut != Shortcut::None {
            self.keyboard_layer = None;
        }
        match shortcut {
            Shortcut::None => {}
            Shortcut::CloseActive => {
                if let Some((client, surface)) = self.stack.active() {
                    self.server
                        .with_client(client, |client| client.close(surface));
                    println!("compositor: close requested");
                }
            }
            Shortcut::CycleWindows { reverse } => {
                if let Some(switcher) = &mut self.switcher {
                    switcher.step(reverse);
                } else {
                    self.open_switcher(reverse);
                }
                self.dirty = true;
            }
            Shortcut::WindowMenu => {
                if let Some((client, surface)) = self.stack.active()
                    && let Some(placement) = self.stack.placement(client, surface).copied()
                {
                    let scale = self.scale as i32;
                    let at = (placement.x + 3 * scale, placement.y + 22 * scale);
                    self.open_menu(placement, at, true);
                }
            }
            Shortcut::LowerActive => {
                if let Some((client, surface)) = self.stack.active() {
                    self.dirty |= self.stack.lower(client, surface);
                }
            }
        }
    }

    /// 键盘焦点跟随活动窗口（或独占键盘的外壳 surface）；窗口布局变化会改变光标下的窗口，位置没动所以不发 motion。
    fn sync_focus(&mut self) {
        let scale = self.scale as i32;
        let boxes = self.layer_boxes();
        let exclusive = self.exclusive_keyboard(&boxes);
        if exclusive.is_some_and(|(client, _)| Some(client) == self.logon_client) {
            self.secure_pending = false;
        }
        self.keyboard_layer = self.keyboard_layer.filter(|target| {
            (self.secure_pending && Some(target.0) == self.logon_client)
                || boxes.iter().any(|layer| {
                    (layer.client, layer.surface) == *target
                        && layer.keyboard == KeyboardInteractivity::OnDemand
                })
        });
        // 最后一个普通窗口（包括外壳启动的独立确认）退出后，后续按键回到桌面。
        // 复用既有 layer 状态，不保留第二套焦点历史；独占、安全握手和显式图层焦点仍优先。
        if exclusive.is_none()
            && !self.secure_pending
            && self.keyboard_layer.is_none()
            && self.stack.active().is_none()
        {
            self.keyboard_layer = self.desktop_keyboard(&boxes);
        }
        self.dirty |= self.seat.sync_keyboard(
            &mut self.server,
            &self.stack,
            scale,
            if self.secure_pending {
                self.keyboard_layer
            } else {
                exclusive.or(self.keyboard_layer)
            },
        );
        let time = self.now_ms();
        let pointer = self.input.pointer().position();
        if self.server.dragging() {
            let hit = self.hit_test(&boxes, pointer);
            self.route_drag(hit, pointer, time, false);
            return;
        }
        let hit = self
            .pointer_grab
            .and_then(|grab| {
                self.locate_target(&boxes, grab.target)
                    .filter(|hit| hit.generation == grab.generation)
            })
            .or_else(|| self.hit_test(&boxes, pointer));
        self.seat
            .update_pointer(&mut self.server, hit, pointer, scale, time, false);
    }

    fn set_mode(&mut self, client: ClientId, surface: ObjectId, mode: Mode) {
        let (work, full) = (self.work_area(), self.full_area());
        let before = self
            .stack
            .placement(client, surface)
            .map(|placed| (placed.x, placed.y, placed.width, placed.height));
        if let Some(size) = self.stack.set_mode(client, surface, mode, work, full) {
            // 窗口几何立即切到目标，画面用整窗缩放从旧矩形过渡到新矩形；
            // 缺少它，旧尺寸的缓冲会先跳到新位置，等客户端提交新尺寸才撑满，观感是先移动再拉伸。
            if let (Some(from), Some(placed)) = (before, self.stack.placement(client, surface)) {
                let to = (placed.x, placed.y, placed.width, placed.height);
                self.start_animation(client, surface, from, to);
            }
            // 回到普通模式时要带上还原尺寸：普通窗口默认的 0×0 会让客户端留在最大化时的大小。
            self.send_configure(client, surface, Some(size), false);
            self.dirty = true;
        }
    }

    /// 把窗口当前应有的尺寸与状态发给客户端；`size` 是缩放抓取的目标物理尺寸。
    fn send_configure(
        &mut self,
        client: ClientId,
        surface: ObjectId,
        size: Option<(i32, i32)>,
        resizing: bool,
    ) {
        self.dirty |= self
            .seat
            .sync_activation(&mut self.server, &self.stack, self.scale as i32);
        let (width, height, state) = window_configure(
            &self.stack,
            (client, surface),
            self.scale as i32,
            size,
            resizing,
        );
        self.server.with_client(client, |client| {
            client.configure(surface, width, height, state)
        });
    }

    /// 抓取期间指针移动：移动窗口，或按被抓住的边缩放并通知客户端新尺寸。
    fn drag(&mut self, pointer: (i32, i32)) {
        let Some(active) = &self.grab else {
            return;
        };
        let (grab, scale) = (active.grab, self.scale as i32);
        match grab.kind {
            Kind::Move => {
                let (x, y) = grab.moved_to(pointer, self.screen(), scale);
                self.dirty |= self.stack.move_to(grab.client, grab.surface, x, y);
            }
            Kind::Resize(_) => {
                let (x, y, width, height) = grab.resized_to(pointer, scale);
                self.dirty |= self.stack.move_to(grab.client, grab.surface, x, y);
                if active.last_size != Some((width, height)) {
                    self.send_configure(grab.client, grab.surface, Some((width, height)), true);
                    if let Some(active) = &mut self.grab {
                        active.last_size = Some((width, height));
                    }
                }
            }
        }
    }

    fn end_grab(&mut self) -> io::Result<()> {
        let Some(active) = self.grab.take() else {
            return Ok(());
        };
        // 缩放结束：告诉客户端不再处于缩放中（保留最后的尺寸）。
        if let (Kind::Resize(_), Some(size)) = (active.grab.kind, active.last_size)
            && self
                .stack
                .placement(active.grab.client, active.grab.surface)
                .is_some_and(|placed| placed.generation == active.grab.generation)
        {
            self.send_configure(active.grab.client, active.grab.surface, Some(size), false);
        }
        self.seat.end_grab();
        self.shape = Shape::Default;
        self.apply_cursor()
    }

    fn process_data_device(&mut self) {
        let previous = self.server.dragging();
        let grab = self
            .pointer_grab
            .filter(|_| !self.secure_pending && self.grab.is_none())
            .map(|grab| (grab.target.0, grab.target.1, grab.serial));
        let time = self.now_ms();
        self.server
            .handle_data_device(self.seat.keyboard_client(), grab, time);
        let dragging = self.server.dragging();
        if dragging && !previous {
            self.pointer_grab = None;
            let pointer = self.input.pointer().position();
            let time = self.now_ms();
            self.seat.update_pointer(
                &mut self.server,
                None,
                pointer,
                self.scale as i32,
                time,
                false,
            );
        }
        self.dirty |= previous != dragging;
    }

    fn route_drag(&mut self, hit: Option<Hit>, pointer: (i32, i32), time: u32, moved: bool) {
        let destination = hit.map(|hit| {
            let (x, y) = local_position(hit.origin, pointer, self.scale as i32);
            (hit.target, x, y)
        });
        self.server
            .drag_motion(destination, self.seat.next_serial(), time, moved);
        self.dirty |= moved && self.server.drag_icon().is_some();
    }

    /// 剪贴板跟随键盘焦点：处理选择请求，焦点换到新客户端时把当前选择告诉它。
    fn sync_selection(&mut self) {
        let focus = self.seat.keyboard_client();
        if focus != self.selection_focus {
            self.selection_focus = focus;
            self.server.announce_selection(focus);
        }
    }

    /// 外壳进程退出时记录；重启策略等 `logon` 的会话设计确定后再做。
    fn check_shell(&mut self) {
        if let Some(shell) = &mut self.shell
            && let Ok(Some(status)) = shell.try_wait()
        {
            println!("compositor: shell exited {status}");
            // 可信登录进程退出时 fail closed；让 init 的 display-service 一并重建显示会话。
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trusted_priority_is_exclusive_to_the_secure_overlay() {
        assert_eq!(
            layer_order(Layer::Background, true),
            layer_order(Layer::Background, false)
        );
        assert!(layer_order(Layer::Overlay, true) > layer_order(Layer::Overlay, false));
        assert!(layer_order(Layer::Overlay, false) > layer_order(Layer::Top, true));
    }
}
