//! 显示会话主循环：输入、Wayland 连接、窗口栈、桌面外壳与整帧合成的唯一装配点。

mod presentation;
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
    unix::{PollEvents, PollFd, poll},
};
use wayland_wire::wire::ObjectId;

use crate::{
    boot::{self, Progress},
    compose::{self, Drawable},
    cursor::{self, CursorSet},
    desktop::{Geometry, Initial, Mode, Stack},
    focus::{Hit, Seat, Shortcut, window_configure},
    input::{self, Event, Input},
    interaction::{Edges, Grab, Kind},
    scanout::Scanout,
    shell_layout,
    wayland::{
        ClientId, ForeignAction, ForeignInfo, ForeignState, KeyboardInteractivity, Layer, Server,
        WindowRequest,
    },
};

/// Linux `BTN_LEFT`（`linux/input-event-codes.h`）。
const BUTTON_LEFT: u16 = 0x110;
const BUTTON_RIGHT: u16 = 0x111;
/// 键盘移动/缩放窗口时方向键每次的步长，逻辑像素。
const KEYBOARD_STEP: i32 = 8;

/// 窗口内容放大到输出 scale 所需的整数倍；客户端 scale 大于输出时不缩小，按 1 处理。
fn factor(output_scale: u32, surface_scale: i32) -> i32 {
    (output_scale as i32 / surface_scale.max(1)).max(1)
}

/// 最小化或还原时在窗口与任务按钮之间移动的矩形框（Windows 的 `DrawAnimatedRects`）。
struct Animation {
    from: (i32, i32, i32, i32),
    to: (i32, i32, i32, i32),
    start: Instant,
}

/// 动画时长。
const ANIMATION: Duration = Duration::from_millis(200);

impl Animation {
    fn finished(&self) -> bool {
        self.start.elapsed() >= ANIMATION
    }

    /// 当前帧的矩形：起止矩形按进度线性插值。
    fn frame(&self) -> (i32, i32, i32, i32) {
        let t = (self.start.elapsed().as_secs_f32() / ANIMATION.as_secs_f32()).clamp(0.0, 1.0);
        let mix = |a: i32, b: i32| a + ((b - a) as f32 * t) as i32;
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
    width: i32,
    height: i32,
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
    },
}

/// 启动画面阶段的状态：画面数据、进度，以及上一次计时与最后一次画出的值。
struct BootScreen {
    scene: Vec<u8>,
    progress: Progress,
    last_tick: Instant,
    painted: u16,
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
    /// `LBS1` 启动画面数据。
    pub boot_scene: Vec<u8>,
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
    last_pointer_focus: Option<(ClientId, ObjectId)>,
    /// 当前按下的指针按钮。非空期间指针焦点固定在按下时的 surface（Wayland 的隐式抓取）：
    /// 否则拖动 layer surface 这类“随指针移动”的界面时，指针一旦跑出 surface 就收不到后续 motion 与释放。
    pressed_buttons: BTreeSet<u16>,
    /// 隐式抓取的目标；所有按钮释放或目标消失时清除。
    pointer_grab: Option<(ClientId, ObjectId)>,
    /// 点击选中的 OnDemand 外壳；与 seat 的实际焦点分开保存策略，在窗口点击或图层消失时撤销。
    keyboard_layer: Option<(ClientId, ObjectId)>,
    /// 合成器拉起的外壳进程（`logon`）；目前只检测它的退出。
    shell: Option<Child>,
    /// compositor 创建的初始连接；仅它拥有 SAS 路由，委派 Shell 不能替代。
    logon_client: Option<ClientId>,
    /// SAS 后直到可信 Overlay 映射的输入屏障；缺少它会把握手期间的输入泄露给旧窗口。
    secure_pending: bool,
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
    minimize_targets: HashMap<(ClientId, ObjectId), (i32, i32, i32, i32)>,
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
    pub fn new(parts: Parts) -> io::Result<Self> {
        let mut session = Self {
            scanout: parts.scanout,
            input: parts.input,
            server: parts.server,
            theme: parts.theme,
            scale: parts.scale,
            cursors: parts.cursors,
            hardware_cursor: parts.hardware_cursor,
            shape: Shape::Default,
            last_pointer_focus: None,
            keyboard_layer: None,
            shell: None,
            logon_client: None,
            secure_pending: false,
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
                last_tick: Instant::now(),
                painted: boot::HANDOFF_PERMILLE,
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
        let flip_busy = self.scanout.flip_pending();
        if boot.progress.permille() != boot.painted && !flip_busy {
            boot::paint(self.scanout.back(), &boot.scene, boot.progress.permille())
                .map_err(io::Error::other)?;
            boot.painted = boot.progress.permille();
            self.scanout.request_flip()?;
        } else if boot.progress.complete() && boot.painted == 1000 && !flip_busy && shell_visible {
            // 外壳已经有内容、进度条也走满并已上屏：交给桌面合成。
            self.boot = None;
            self.dirty = true;
            println!("compositor: boot complete");
            return Ok(None);
        }
        let waiting = boot.progress.animating() || boot.progress.permille() != boot.painted;
        Ok(waiting.then_some(Duration::from_millis(16)))
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
                let factor = factor(self.scale, layer.scale);
                let size = (
                    layer.content.width as i32 * factor,
                    layer.content.height as i32 * factor,
                );
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
                    width: size.0,
                    height: size.1,
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

    /// 隐式抓取目标当前的位置；目标是外壳 surface 或应用窗口时返回，其他（弹出层）或已消失时 `None`。
    fn locate_target(&self, boxes: &[LayerBox], target: (ClientId, ObjectId)) -> Option<Hit> {
        boxes
            .iter()
            .find(|layer| (layer.client, layer.surface) == target)
            .map(|layer| (layer.x, layer.y))
            .or_else(|| {
                self.stack
                    .windows()
                    .iter()
                    .find(|placed| (placed.client, placed.surface) == target)
                    .map(|placed| (placed.x, placed.y))
            })
            .map(|origin| Hit { target, origin })
    }

    /// 指针下最上面的 surface：Overlay、Top 层，然后是应用窗口，最后是 Bottom、Background 层。
    /// 弹出层的命中：位置相对根窗口，最后创建的（最上面的）优先。
    fn popup_hit(&self, point: (i32, i32)) -> Option<Hit> {
        let scale = self.scale as i32;
        self.server
            .popups()
            .into_iter()
            .rev()
            .find_map(|(client, popup)| {
                let root = self.stack.placement(client, popup.client_root)?;
                let factor = factor(self.scale, popup.scale);
                let (x, y) = (root.x + popup.x * scale, root.y + popup.y * scale);
                let (width, height) = (
                    popup.content.width as i32 * factor,
                    popup.content.height as i32 * factor,
                );
                ((x..x + width).contains(&point.0) && (y..y + height).contains(&point.1)).then_some(
                    Hit {
                        target: (client, popup.surface),
                        origin: (x, y),
                    },
                )
            })
    }

    fn hit_test(&self, boxes: &[LayerBox], point: (i32, i32)) -> Option<Hit> {
        if self.secure_pending {
            return None;
        }
        let layer_hit = |wanted: &dyn Fn(Layer) -> bool| {
            boxes
                .iter()
                .rev()
                .filter(|layer| wanted(layer.layer))
                .find(|layer| {
                    (layer.x..layer.x + layer.width).contains(&point.0)
                        && (layer.y..layer.y + layer.height).contains(&point.1)
                })
                .map(|layer| Hit {
                    target: (layer.client, layer.surface),
                    origin: (layer.x, layer.y),
                })
        };
        layer_hit(&|layer| layer >= Layer::Top)
            .or_else(|| self.popup_hit(point))
            .or_else(|| {
                self.stack.window_at(point.0, point.1).map(|placed| Hit {
                    target: (placed.client, placed.surface),
                    origin: (placed.x, placed.y),
                })
            })
            .or_else(|| layer_hit(&|layer| layer < Layer::Top))
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

    /// 运行到出错为止；正常情况下不返回。
    ///
    /// # Errors
    ///
    /// DRM、输入或监听 socket 的不可恢复错误。
    pub fn run(mut self) -> io::Result<()> {
        let mut events = Vec::new();
        loop {
            let shell_visible =
                !self.stack.windows().is_empty() || self.server.layers().next().is_some();
            if shell_visible {
                // 外壳第一个 surface 出现：这是启动完成的真实信号。
                self.boot_milestone(1000);
            }
            let mut timeout = self.step_boot(shell_visible)?;
            if let Some(animation) = &self.animation {
                if animation.finished() {
                    self.animation = None;
                } else {
                    timeout = Some(Duration::from_millis(16));
                }
                self.dirty = true;
            }
            if self.dirty && self.boot.is_none() && !self.scanout.flip_pending() {
                self.present()?;
                self.dirty = false;
            }
            let input_count = self.input.devices().count();
            let (drm_ready, input_ready, server_events) = {
                let mut descriptors =
                    vec![PollFd::new(self.scanout.device().as_fd(), PollEvents::READ)];
                descriptors.extend(
                    self.input
                        .devices()
                        .map(|device| PollFd::new(device.file().as_fd(), PollEvents::READ)),
                );
                descriptors.extend(self.server.descriptors());
                poll(&mut descriptors, timeout)?;
                let returned: Vec<PollEvents> = descriptors.iter().map(PollFd::returned).collect();
                let ready = |events: &PollEvents| events.contains(PollEvents::READ);
                (
                    ready(&returned[0]),
                    returned[1..=input_count]
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| ready(e))
                        .map(|(i, _)| i)
                        .collect::<Vec<_>>(),
                    returned[1 + input_count..].to_vec(),
                )
            };
            if drm_ready && self.scanout.flip_pending() {
                self.scanout.complete_flip()?;
                let time = self.now_ms();
                self.server.finish_frame(time);
            }
            for index in input_ready {
                self.input.drain(index, &mut events)?;
            }
            // 一批积压的位置只应用最终状态，避免逐样本提交光标与 resize configure。
            input::coalesce_motion(&mut events);
            for event in events.drain(..) {
                self.on_input(event)?;
            }
            self.server.dispatch(&server_events)?;
            self.sync_windows()?;
            self.sync_work_area();
            self.handle_requests()?;
            self.handle_foreign_requests();
            self.sync_selection();
            self.sync_foreign();
            self.sync_focus();
            self.handle_cursor_requests()?;
            self.check_shell();
        }
    }

    fn on_input(&mut self, event: Event) -> io::Result<()> {
        let time = self.now_ms();
        let scale = self.scale as i32;
        match event {
            Event::PointerMoved { x, y } => {
                self.scanout.move_cursor((x, y))?;
                if !std::mem::replace(&mut self.saw_pointer, true) {
                    println!("compositor: first pointer event {x},{y}");
                }
                if let Some(menu) = &mut self.window_menu {
                    self.dirty |= menu.hover_at((x, y));
                } else if self.grab.is_some() {
                    self.drag((x, y));
                } else {
                    let boxes = self.layer_boxes();
                    let grabbed = self
                        .pointer_grab
                        .and_then(|target| self.locate_target(&boxes, target));
                    if grabbed.is_none() {
                        self.pointer_grab = None;
                    }
                    let hit = grabbed.or_else(|| self.hit_test(&boxes, (x, y)));
                    self.seat
                        .update_pointer(&mut self.server, hit, (x, y), scale, time, true);
                }
            }
            Event::Button { code, pressed } => {
                // 在任何提前返回之前维护按钮集合，释放才不会漏掉而让抓取悬挂。
                if pressed {
                    if self.pressed_buttons.is_empty() {
                        self.pointer_grab = self.seat.pointer_focus();
                    }
                    self.pressed_buttons.insert(code);
                } else {
                    self.pressed_buttons.remove(&code);
                    if self.pressed_buttons.is_empty() {
                        self.pointer_grab = None;
                    }
                }
                // 点击别处取消 Alt+Tab 面板；窗口仍留在原来的位置。
                if pressed && self.switcher.take().is_some() {
                    self.dirty = true;
                }
                if self.window_menu.is_some() {
                    if pressed && matches!(code, BUTTON_LEFT | BUTTON_RIGHT) {
                        self.menu_click()?;
                    }
                    return Ok(());
                }
                if self.grab.as_ref().is_some_and(|active| active.sticky) {
                    if code == BUTTON_LEFT && pressed {
                        self.end_grab()?;
                    }
                    return Ok(());
                }
                if code == BUTTON_LEFT && !pressed && self.grab.is_some() {
                    self.end_grab()?;
                } else if code == BUTTON_LEFT && pressed {
                    // 点在弹出层之外：持有 grab 的弹出层结束（客户端随后销毁它们）。
                    let point = self.input.pointer().position();
                    if self.popup_hit(point).is_none() {
                        let clients: Vec<ClientId> = self
                            .server
                            .popups()
                            .into_iter()
                            .map(|(client, _)| client)
                            .collect();
                        for client in clients {
                            self.server.dismiss_grabbed_popups(client);
                        }
                    }
                    // 单击应用窗口：激活并置顶。外壳 surface 不是窗口，点击它不改变活动窗口。
                    let point = self.input.pointer().position();
                    let boxes = self.layer_boxes();
                    let hit = self.hit_test(&boxes, point);
                    if !self.secure_pending {
                        self.keyboard_layer = hit
                            .and_then(|hit| {
                                boxes
                                    .iter()
                                    .find(|layer| (layer.client, layer.surface) == hit.target)
                            })
                            .filter(|layer| layer.keyboard == KeyboardInteractivity::OnDemand)
                            .map(|layer| (layer.client, layer.surface));
                    }
                    let covered = hit.is_some_and(|hit| {
                        boxes.iter().any(|layer| {
                            (layer.client, layer.surface) == hit.target && layer.layer >= Layer::Top
                        })
                    });
                    if !self.secure_pending
                        && !covered
                        && let Some(target) = self
                            .stack
                            .window_at(point.0, point.1)
                            .map(|placed| (placed.client, placed.surface))
                        && self.stack.raise(target.0, target.1)
                    {
                        self.dirty = true;
                    }
                }
                if !self.secure_pending {
                    self.seat.button(&mut self.server, code, pressed, time);
                }
            }
            Event::Wheel {
                vertical,
                horizontal,
            } => {
                // 窗口菜单、拖动抓取期间滚轮不交给应用。
                if self.window_menu.is_none() && self.grab.is_none() && !self.secure_pending {
                    self.seat
                        .wheel(&mut self.server, vertical, horizontal, time);
                }
            }
            Event::Key { code, pressed } => {
                if !std::mem::replace(&mut self.saw_key, true) {
                    println!("compositor: first key event");
                }
                // SAS 只识别硬件输入；登录背景始终映射，因此使用标准 wl_keyboard 焦点与按键事件。
                // 先撤销抓取，防止安全界面出现后旧窗口仍占有鼠标。
                if self.seat.secure_attention(code, pressed) {
                    self.close_menu();
                    if self.grab.is_some() {
                        self.end_grab()?;
                    }
                    self.secure_pending = self.logon_client.is_some();
                    self.keyboard_layer = self
                        .layer_boxes()
                        .iter()
                        .find(|layer| Some(layer.client) == self.logon_client)
                        .map(|layer| (layer.client, layer.surface));
                }
                if self.modal_key(code, pressed)? {
                    return Ok(());
                }
                // 键盘焦点必须先于这次按键同步：刚激活的窗口应该收到触发它之后的第一个键。
                let exclusive = self.exclusive_keyboard(&self.layer_boxes());
                if exclusive.is_none()
                    && !self.secure_pending
                    && self.seat.desktop_shortcut(code, pressed)
                {
                    self.keyboard_layer = self
                        .layer_boxes()
                        .iter()
                        .find(|layer| {
                            Some(layer.client) != self.logon_client
                                && layer.layer == Layer::Background
                                && layer.keyboard == KeyboardInteractivity::OnDemand
                        })
                        .map(|layer| (layer.client, layer.surface));
                }
                self.seat.sync_keyboard(
                    &mut self.server,
                    &self.stack,
                    scale,
                    if self.secure_pending {
                        self.keyboard_layer
                    } else {
                        exclusive.or(self.keyboard_layer)
                    },
                );
                // Alt 的 evdev 键码：左 56、右 100。
                if !pressed && matches!(code, 56 | 100) {
                    self.commit_switcher();
                }
                let shortcut = self.seat.key(
                    &mut self.server,
                    code,
                    pressed,
                    time,
                    exclusive.is_none()
                        && !self.secure_pending
                        && !(code == 62 && self.keyboard_layer.is_some()),
                );
                // 登录屏独占键盘时，窗口管理快捷键（Alt+Tab、Alt+F4…）不能越过它。
                if exclusive.is_none() {
                    self.on_shortcut(shortcut);
                }
            }
        }
        Ok(())
    }

    /// 在物理坐标 `at` 打开窗口菜单，并让窗口看到指针离开。
    fn open_menu(&mut self, placement: crate::desktop::Placement, at: (i32, i32), cues: bool) {
        self.ensure_art();
        self.keyboard_layer = None;
        self.seat.begin_grab(&mut self.server);
        self.window_menu = Some(WindowMenu::open(
            &placement,
            at,
            self.screen(),
            self.scale as i32,
            cues,
        ));
        self.dirty = true;
    }

    /// 关闭窗口菜单并把指针焦点还给窗口。
    fn close_menu(&mut self) {
        if self.window_menu.take().is_none() {
            return;
        }
        self.seat.end_grab();
        let (scale, time) = (self.scale as i32, self.now_ms());
        let pointer = self.input.pointer().position();
        let boxes = self.layer_boxes();
        let hit = self.hit_test(&boxes, pointer);
        self.seat
            .update_pointer(&mut self.server, hit, pointer, scale, time, false);
        self.dirty = true;
    }

    /// 菜单打开时的一次点击：执行命中的命令，点在菜单外则关闭。
    fn menu_click(&mut self) -> io::Result<()> {
        let point = self.input.pointer().position();
        let Some(menu) = &self.window_menu else {
            return Ok(());
        };
        let (client, surface) = (menu.client, menu.surface);
        match menu.click(point) {
            Ok(command) => {
                self.close_menu();
                self.run_menu_command(client, surface, command)?;
            }
            Err(true) => {}
            Err(false) => self.close_menu(),
        }
        Ok(())
    }

    fn run_menu_command(
        &mut self,
        client: ClientId,
        surface: ObjectId,
        command: window_menu::Command,
    ) -> io::Result<()> {
        use window_menu::Command;
        let Some(placement) = self.stack.placement(client, surface).copied() else {
            return Ok(());
        };
        match command {
            Command::Restore if placement.minimized => self.restore_window(client, surface),
            Command::Restore => self.set_mode(client, surface, Mode::Normal),
            Command::Minimize => self.minimize_window(client, surface),
            Command::Maximize => self.set_mode(client, surface, Mode::Maximized),
            Command::Close => self
                .server
                .with_client(client, |client| client.close(surface)),
            Command::Move | Command::Size => {
                let kind = if command == Command::Move {
                    Kind::Move
                } else {
                    Kind::Resize(Edges {
                        left: false,
                        right: true,
                        top: false,
                        bottom: true,
                    })
                };
                let start = (placement.x, placement.y, placement.width, placement.height);
                self.seat.begin_grab(&mut self.server);
                self.grab = Some(ActiveGrab {
                    grab: Grab {
                        client,
                        surface,
                        kind,
                        pointer: self.input.pointer().position(),
                        start,
                    },
                    last_size: None,
                    sticky: true,
                    origin: start,
                });
                if let Kind::Resize(edges) = kind {
                    self.shape = Shape::Named(cursor::resize_cursor_name(edges));
                    self.apply_cursor()?;
                }
            }
        }
        Ok(())
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

    /// 开始最小化（`minimizing`）或还原的动画：窗口矩形与任务按钮矩形之间。没有任务按钮位置时取屏幕底边中部。
    fn animate(&mut self, client: ClientId, surface: ObjectId, minimizing: bool) {
        let Some(placement) = self.stack.placement(client, surface).copied() else {
            return;
        };
        let window = (placement.x, placement.y, placement.width, placement.height);
        let scale = self.scale as i32;
        let (screen_width, screen_height) = self.screen();
        let target = self
            .minimize_targets
            .get(&(client, surface))
            .copied()
            .unwrap_or((
                screen_width / 2 - 50 * scale,
                screen_height - 28 * scale,
                100 * scale,
                28 * scale,
            ));
        let (from, to) = if minimizing {
            (window, target)
        } else {
            (target, window)
        };
        self.animation = Some(Animation {
            from,
            to,
            start: Instant::now(),
        });
        self.dirty = true;
    }

    /// 最小化窗口并播放动画。
    fn minimize_window(&mut self, client: ClientId, surface: ObjectId) {
        if self
            .stack
            .placement(client, surface)
            .is_some_and(|placed| !placed.minimized)
        {
            self.animate(client, surface, true);
        }
        self.dirty |= self.stack.minimize(client, surface);
    }

    /// 激活窗口；它原来是最小化的就播放还原动画。
    fn restore_window(&mut self, client: ClientId, surface: ObjectId) {
        if self
            .stack
            .placement(client, surface)
            .is_some_and(|placed| placed.minimized)
        {
            self.animate(client, surface, false);
        }
        self.dirty |= self.stack.raise(client, surface);
    }

    /// 以最近使用顺序（栈顶在前）列出全部窗口并打开切换面板。
    fn open_switcher(&mut self, reverse: bool) {
        let entries = self
            .stack
            .windows()
            .iter()
            .rev()
            .map(|placed| {
                let window = self.server.windows().find(|(client, window)| {
                    *client == placed.client && window.surface == placed.surface
                });
                switcher::Entry {
                    client: placed.client,
                    surface: placed.surface,
                    title: window
                        .as_ref()
                        .map_or_else(String::new, |(_, w)| w.title.to_owned()),
                    app_id: window
                        .as_ref()
                        .map_or_else(String::new, |(_, w)| w.app_id.to_owned()),
                }
            })
            .collect();
        self.switcher = Switcher::open(entries, reverse);
        if self.switcher.is_some() {
            self.ensure_art();
        }
    }

    /// 松开 Alt：激活选中的窗口并收起面板。
    fn commit_switcher(&mut self) {
        if let Some(switcher) = self.switcher.take() {
            let (client, surface) = switcher.target();
            self.restore_window(client, surface);
        }
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

    /// 与客户端的已映射窗口对账；窗口增减、尺寸变化与新的 commit 都会触发重绘。
    fn sync_windows(&mut self) -> io::Result<()> {
        let mapped: Vec<(ClientId, ObjectId, i32, i32, Initial)> = self
            .server
            .windows()
            .map(|(client, window)| {
                let factor = factor(self.scale, window.scale);
                (
                    client,
                    window.surface,
                    window.content.width as i32 * factor,
                    window.content.height as i32 * factor,
                    match (window.parent, window.fixed) {
                        (Some(parent), _) => Initial::CenterOn(parent),
                        (None, true) => Initial::Center,
                        (None, false) => Initial::Cascade,
                    },
                )
            })
            .collect();
        let before = self.stack.windows().len();
        let active_before = self.stack.active();
        if self.stack.sync(&mapped, self.screen(), self.scale as i32) {
            // 映射/退出改变活动窗口时，焦点跟随窗口栈；SAS 握手仍由可信登录目标独占。
            if self.stack.active() != active_before && !self.secure_pending {
                self.keyboard_layer = None;
            }
            self.dirty = true;
            if self.stack.windows().len() != before {
                println!("compositor: windows={}", self.stack.windows().len());
                for (_, window) in self.server.windows() {
                    println!(
                        "compositor: window {:?} app_id={:?}",
                        window.title, window.app_id
                    );
                }
            }
        }
        let layer_count = self.server.layers().count();
        if layer_count != self.layers_seen {
            self.layers_seen = layer_count;
            self.dirty = true;
            for (_, layer) in self.server.layers() {
                println!(
                    "compositor: layer {:?} {:?}",
                    layer.namespace, layer.state.layer
                );
            }
        }
        let commits = self.server.commits();
        if commits != self.commits_seen {
            self.commits_seen = commits;
            self.dirty = true;
        }
        // 抓取的窗口消失了（被关闭、客户端断开）：抓取随之结束。
        if let Some(active) = &self.grab
            && self
                .stack
                .placement(active.grab.client, active.grab.surface)
                .is_none()
        {
            self.end_grab()?;
        }
        Ok(())
    }

    /// 任务栏等独占区出现、消失或改变大小：最大化的窗口重新铺满新的工作区。
    fn sync_work_area(&mut self) {
        let (work, full) = (self.work_area(), self.full_area());
        for (client, surface, size) in self.stack.refit(work, full) {
            self.send_configure(client, surface, Some(size), false);
            self.dirty = true;
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
        self.seat.sync_keyboard(
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
        let hit = self.hit_test(&boxes, pointer);
        self.seat
            .update_pointer(&mut self.server, hit, pointer, scale, time, false);
    }

    /// 处理客户端的窗口请求：交互式移动/缩放、最大化、全屏与最小化。
    fn handle_requests(&mut self) -> io::Result<()> {
        for (client, surface, request) in self.server.take_requests() {
            let Some(placement) = self.stack.placement(client, surface).copied() else {
                continue;
            };
            match request {
                WindowRequest::Move { serial } | WindowRequest::Resize { serial, .. } => {
                    // 只接受由仍按住的按钮触发的请求，且窗口不能是最大化/全屏，也不能已有抓取。
                    let valid = self.seat.button_serial() == Some(serial)
                        && placement.mode == Mode::Normal
                        && self.grab.is_none();
                    let kind = match request {
                        WindowRequest::Resize { edges, .. } => {
                            Edges::from_xdg(edges).map(Kind::Resize)
                        }
                        _ => Some(Kind::Move),
                    };
                    if let (true, Some(kind)) = (valid, kind) {
                        let start = (placement.x, placement.y, placement.width, placement.height);
                        let grab = Grab {
                            client,
                            surface,
                            kind,
                            pointer: self.input.pointer().position(),
                            start,
                        };
                        self.seat.begin_grab(&mut self.server);
                        self.grab = Some(ActiveGrab {
                            grab,
                            last_size: None,
                            sticky: false,
                            origin: start,
                        });
                        if let Kind::Resize(edges) = kind {
                            self.shape = Shape::Named(cursor::resize_cursor_name(edges));
                            self.apply_cursor()?;
                        }
                    }
                }
                WindowRequest::Maximize(on) => {
                    self.set_mode(
                        client,
                        surface,
                        if on { Mode::Maximized } else { Mode::Normal },
                    );
                }
                WindowRequest::Fullscreen(on) => {
                    self.set_mode(
                        client,
                        surface,
                        if on { Mode::Fullscreen } else { Mode::Normal },
                    );
                }
                WindowRequest::Minimize => self.minimize_window(client, surface),
                WindowRequest::ShowMenu { serial, x, y } => {
                    if self.seat.button_serial() == Some(serial) && self.window_menu.is_none() {
                        let scale = self.scale as i32;
                        self.open_menu(
                            placement,
                            (placement.x + x * scale, placement.y + y * scale),
                            false,
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// 处理外壳（任务栏）通过 foreign-toplevel 对窗口提出的操作。
    fn handle_foreign_requests(&mut self) {
        for request in self.server.take_foreign_requests() {
            let (client, surface) = (request.client, request.surface);
            if self.stack.placement(client, surface).is_none() {
                continue;
            }
            // 任务栏操作交还窗口焦点；SAS 期间不能撤销可信目标，否则握手中的按键会回落到旧应用。
            if !self.secure_pending {
                self.keyboard_layer = None;
            }
            match request.action {
                ForeignAction::Activate | ForeignAction::Minimize(false) => {
                    self.restore_window(client, surface);
                }
                ForeignAction::Minimize(true) => self.minimize_window(client, surface),
                ForeignAction::Rectangle {
                    surface: shell_surface,
                    x,
                    y,
                    width,
                    height,
                } => {
                    // 矩形是外壳 surface 内的逻辑坐标，换算到输出物理坐标。
                    let scale = self.scale as i32;
                    if let Some(layer) = self.layer_boxes().iter().find(|layer| {
                        layer.client == request.shell && layer.surface == shell_surface
                    }) {
                        self.minimize_targets.insert(
                            (client, surface),
                            (
                                layer.x + x * scale,
                                layer.y + y * scale,
                                width * scale,
                                height * scale,
                            ),
                        );
                    }
                }
                ForeignAction::Maximize(on) => {
                    self.set_mode(
                        client,
                        surface,
                        if on { Mode::Maximized } else { Mode::Normal },
                    );
                }
                ForeignAction::Fullscreen(on) => {
                    self.set_mode(
                        client,
                        surface,
                        if on { Mode::Fullscreen } else { Mode::Normal },
                    );
                }
                ForeignAction::Close => self
                    .server
                    .with_client(client, |client| client.close(surface)),
            }
        }
    }

    /// 让外壳看到的窗口列表与当前窗口栈一致。
    fn sync_foreign(&mut self) {
        let titles: HashMap<(ClientId, ObjectId), (String, String)> = self
            .server
            .windows()
            .map(|(client, window)| {
                (
                    (client, window.surface),
                    (window.title.to_owned(), window.app_id.to_owned()),
                )
            })
            .collect();
        let active = self.stack.active();
        let snapshot: Vec<ForeignInfo> = self
            .stack
            .windows()
            .iter()
            .filter_map(|placed| {
                let (title, app_id) = titles.get(&(placed.client, placed.surface))?.clone();
                Some(ForeignInfo {
                    client: placed.client,
                    surface: placed.surface,
                    title,
                    app_id,
                    state: ForeignState {
                        maximized: placed.mode == Mode::Maximized,
                        minimized: placed.minimized,
                        activated: active == Some((placed.client, placed.surface)),
                        fullscreen: placed.mode == Mode::Fullscreen,
                    },
                })
            })
            .collect();
        self.server.sync_foreign(&snapshot);
    }

    fn set_mode(&mut self, client: ClientId, surface: ObjectId, mode: Mode) {
        let (work, full) = (self.work_area(), self.full_area());
        if let Some(size) = self.stack.set_mode(client, surface, mode, work, full) {
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
        if let (Kind::Resize(_), Some(size)) = (active.grab.kind, active.last_size) {
            let (width, height, state) = window_configure(
                &self.stack,
                (active.grab.client, active.grab.surface),
                self.scale as i32,
                Some(size),
                false,
            );
            self.server.with_client(active.grab.client, |client| {
                client.configure(active.grab.surface, width, height, state)
            });
        }
        self.seat.end_grab();
        self.shape = Shape::Default;
        self.apply_cursor()
    }

    /// 客户端的 `set_cursor`：只接受当前指针焦点客户端、带最近一次 enter 序号的请求。
    fn handle_cursor_requests(&mut self) -> io::Result<()> {
        let focus = self.seat.pointer_focus();
        if focus != self.last_pointer_focus {
            // 指针换了窗口：新窗口的光标由它自己在 enter 之后设置，在此之前用默认光标。
            self.last_pointer_focus = focus;
            if self.grab.is_none() {
                self.shape = Shape::Default;
                self.apply_cursor()?;
            }
        }
        let enter_serial = self.seat.pointer_enter_serial();
        for (client, request) in self.server.take_cursor_requests() {
            let current =
                focus.is_some_and(|(owner, _)| owner == client) && request.serial == enter_serial;
            if !current || self.grab.is_some() {
                continue;
            }
            self.shape = match (request.named, request.surface) {
                (Some(name), _) => Shape::Named(name),
                (None, Some(surface)) => Shape::Client {
                    client,
                    surface,
                    hotspot: request.hotspot,
                },
                (None, None) => Shape::Hidden,
            };
            self.apply_cursor()?;
        }
        // 客户端光标 surface 重新提交了图像（动画光标、延迟提交的第一帧）。
        if matches!(self.shape, Shape::Client { .. }) && self.dirty {
            self.apply_cursor()?;
        }
        Ok(())
    }

    /// 把当前光标形状写进硬件光标。
    fn apply_cursor(&mut self) -> io::Result<()> {
        let position = self.input.pointer().position();
        let scale = self.scale;
        match self.shape {
            Shape::Hidden => return self.scanout.hide_cursor(position),
            Shape::Client {
                client,
                surface,
                hotspot,
            } => match self.server.cursor_image(client, surface) {
                // 无 buffer 的光标 surface 不可见；后续 commit 再更新硬件图像。
                None => return self.scanout.hide_cursor(position),
                Some((content, surface_scale)) => {
                    let factor = factor(scale, surface_scale) as usize;
                    let hot = (
                        hotspot.0.checked_mul(scale as i32),
                        hotspot.1.checked_mul(scale as i32),
                    );
                    if content.width * factor <= cursor::HARDWARE_EDGE
                        && content.height * factor <= cursor::HARDWARE_EDGE
                        && matches!(hot, (Some(x), Some(y)) if (0..64).contains(&x) && (0..64).contains(&y))
                    {
                        cursor::blit_scaled(
                            &mut self.hardware_cursor,
                            &content.pixels,
                            content.width,
                            content.height,
                            factor,
                        )?;
                        let hot = (hot.0.unwrap() as u32, hot.1.unwrap() as u32);
                        self.scanout
                            .show_cursor(&self.hardware_cursor, position, hot)?;
                        println!(
                            "compositor: client cursor {}x{} hotspot {},{}",
                            content.width, content.height, hot.0, hot.1
                        );
                        return Ok(());
                    }
                    // 硬件光标最大 64×64；更大的图像退回默认光标。
                }
            },
            Shape::Default | Shape::Named(_) => {}
        }
        let name = match self.shape {
            Shape::Named(name) => name,
            _ => "arrow",
        };
        let image = self
            .cursors
            .get(name)
            .or_else(|| self.cursors.get("arrow"))
            .ok_or_else(|| io::Error::other("cursor set has no `arrow`"))?;
        let (edge, frame) = image.frame(scale, 0);
        cursor::blit_to_hardware(&mut self.hardware_cursor, edge, frame)?;
        self.scanout
            .show_cursor(&self.hardware_cursor, position, image.hotspot(scale))
    }

    /// 剪贴板跟随键盘焦点：处理选择请求，焦点换到新客户端时把当前选择告诉它。
    fn sync_selection(&mut self) {
        let focus = self.seat.keyboard_client();
        self.server.handle_selection(focus);
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
