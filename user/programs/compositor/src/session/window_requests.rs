//! 对账已提交映射并按连接线序裁决窗口与外壳事务；窗口栈是批量成员和桌面状态的唯一 owner。

use super::*;

impl Session {
    /// 与客户端的已映射窗口对账；窗口增减、尺寸变化与新的 commit 都会触发重绘。
    pub(super) fn sync_windows(&mut self) -> io::Result<bool> {
        let server = &self.server;
        self.stack.retain_prepared(|client, surface, generation| {
            server.toplevel_state(client, surface, generation).is_some()
        });
        let mapped: Vec<(ClientId, ObjectId, u64, i32, i32, Initial)> = self
            .server
            .windows()
            .map(|(client, window)| {
                (
                    client,
                    window.surface,
                    window.generation,
                    window.content.output_size(self.scale as i32).0,
                    window.content.output_size(self.scale as i32).1,
                    match (window.parent, window.fixed) {
                        (Some(parent), _) => Initial::CenterOn(parent),
                        (None, true) => Initial::Center,
                        (None, false) => Initial::Cascade,
                    },
                )
            })
            .collect();
        let modal: Vec<_> = self
            .server
            .windows()
            .filter_map(|(client, window)| {
                window
                    .parent
                    .filter(|_| window.modal)
                    .map(|parent| (client, parent, window.surface))
            })
            .collect();
        let before = self.stack.windows().len();
        let active_before = self
            .stack
            .active()
            .and_then(|target| self.stack.placement(target.0, target.1).copied());
        let changed = self.stack.sync(&mapped, self.screen(), self.scale as i32);
        if changed {
            // 映射/退出改变活动窗口时，焦点跟随窗口栈；SAS 握手仍由可信登录目标独占。
            if self
                .stack
                .active()
                .and_then(|target| self.stack.placement(target.0, target.1))
                .map(|placed| (placed.client, placed.surface, placed.generation))
                != active_before.map(|placed| (placed.client, placed.surface, placed.generation))
                && !self.secure_pending
            {
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
        // 模态提示只约束其父窗口；其他应用仍可激活。多层对话框逐层将焦点推到最深处。
        for _ in 0..modal.len() {
            let active = self.stack.active();
            let Some((client, _, child)) = modal
                .iter()
                .find(|(client, parent, _)| active == Some((*client, *parent)))
            else {
                break;
            };
            if !self.stack.raise(*client, *child) {
                break;
            }
            if !self.secure_pending {
                self.keyboard_layer = None;
            }
            self.dirty = true;
        }
        if changed {
            // 动画端点只属于被外壳宣告的那次映射；退休后释放，不能持续积累旧 surface 编号。
            self.minimize_targets
                .retain(|(client, surface, generation), _| {
                    self.stack
                        .placement(*client, *surface)
                        .is_some_and(|placed| placed.generation == *generation)
                });
        }
        let layer_count = self.server.layers().count();
        let layers_changed = layer_count != self.layers_seen;
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
        if self.animation.as_ref().is_some_and(|animation| {
            self.stack
                .placement(animation.client, animation.surface)
                .is_none_or(|placed| placed.generation != animation.generation)
        }) {
            self.animation = None;
            self.dirty = true;
        }
        // 抓取的窗口消失了（被关闭、客户端断开）：抓取随之结束。
        if let Some(active) = &self.grab
            && self
                .stack
                .placement(active.grab.client, active.grab.surface)
                .is_none_or(|placed| placed.generation != active.grab.generation)
        {
            self.end_grab()?;
        }
        if self.window_menu.as_ref().is_some_and(|menu| {
            self.stack
                .placement(menu.client, menu.surface)
                .is_none_or(|placed| placed.generation != menu.generation)
        }) {
            self.close_menu();
        }
        if let Some(grab) = self.pointer_grab {
            let root = self.root_target(grab.target);
            let generation = root
                .and_then(|root| self.stack.placement(root.0, root.1))
                .map(|placed| placed.generation);
            if root.is_none() || generation != grab.generation {
                self.pointer_grab = None;
            }
        }
        Ok(changed || layers_changed)
    }

    /// 任务栏等独占区出现、消失或改变大小：最大化的窗口重新铺满新的工作区。
    pub(super) fn sync_work_area(&mut self) {
        let (work, full) = (self.work_area(), self.full_area());
        for (client, surface, size) in self.stack.refit(work, full) {
            self.send_configure(client, surface, Some(size), false);
            self.dirty = true;
        }
    }

    /// 处理客户端的窗口请求：交互式移动/缩放、最大化、全屏与最小化。
    pub(super) fn handle_requests(&mut self) -> io::Result<()> {
        for (client, command) in self.server.take_requests() {
            let (surface, generation, request) = match command {
                WindowCommand::Toplevel(surface, generation, request) => {
                    (surface, generation, request)
                }
                WindowCommand::Foreign(mut request) => {
                    request.shell = client;
                    self.handle_foreign_request(request);
                    continue;
                }
                WindowCommand::Arrange(layout) => {
                    if self.server.has_shell_control(client) {
                        self.arrange_windows(client, Some(layout))?;
                    }
                    continue;
                }
                WindowCommand::UndoArrange => {
                    if self.server.has_shell_control(client) {
                        self.arrange_windows(client, None)?;
                    }
                    continue;
                }
                WindowCommand::Shell(action) => {
                    if self.server.has_shell_control(client) {
                        self.shell_command(client, action);
                    }
                    continue;
                }
            };
            let Some(placement) = self
                .stack
                .placement(client, surface)
                .copied()
                .filter(|placed| placed.generation == generation)
            else {
                let areas = (self.work_area(), self.full_area());
                let scale = self.scale as i32;
                let stack = &mut self.stack;
                self.server.with_client(client, |connection| {
                    unmapped_request(
                        connection,
                        stack,
                        (client, surface, generation),
                        request,
                        areas,
                        scale,
                    );
                });
                continue;
            };
            match request {
                WindowRequest::Initialize => {}
                WindowRequest::Move { serial } | WindowRequest::Resize { serial, .. } => {
                    // 只接受由仍按住的按钮触发的请求，且窗口不能是最大化/全屏，也不能已有抓取。
                    let pressed = self.pointer_grab.filter(|grab| {
                        grab.serial == serial
                            && self.root_target(grab.target) == Some((client, surface))
                            && placement.mode == Mode::Normal
                            && self.grab.is_none()
                    });
                    let kind = match request {
                        WindowRequest::Resize { edges, .. } => {
                            Edges::from_xdg(edges).map(Kind::Resize)
                        }
                        _ => Some(Kind::Move),
                    };
                    if let (Some(pressed), Some(kind)) = (pressed, kind) {
                        let start = (placement.x, placement.y, placement.width, placement.height);
                        let grab = Grab {
                            client,
                            surface,
                            generation,
                            kind,
                            pointer: pressed.position,
                            start,
                        };
                        self.stack.forget_arranged(client, surface);
                        self.seat.begin_grab(&mut self.server);
                        self.grab = Some(ActiveGrab {
                            grab,
                            last_size: None,
                            sticky: false,
                            origin: start,
                        });
                        // motion 可先于客户端请求到达；立即补上从按下锚点到当前位置的位移。
                        self.drag(self.input.pointer().position());
                        if let Kind::Resize(edges) = kind {
                            self.shape = Shape::Named(cursor::resize_cursor_name(edges));
                            self.apply_cursor()?;
                        }
                    }
                }
                WindowRequest::Maximize(_) | WindowRequest::Fullscreen(_) => {
                    if let Some(mode) = requested_mode(placement.mode, request) {
                        self.set_mode(client, surface, mode);
                    }
                }
                WindowRequest::Minimize => self.minimize_window(client, surface),
                WindowRequest::ShowMenu { serial, x, y } => {
                    if self.pointer_grab.is_some_and(|grab| {
                        grab.serial == serial
                            && self.root_target(grab.target) == Some((client, surface))
                    }) && self.window_menu.is_none()
                    {
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
    fn handle_foreign_request(&mut self, request: crate::wayland::ForeignRequest) {
        let (client, surface) = (request.client, request.surface);
        if !self
            .stack
            .placement(client, surface)
            .is_some_and(|placed| placed.generation == request.generation)
        {
            return;
        }
        // 窗口操作交还焦点。矩形只更新动画端点，不能抢走桌面刚获得的键盘焦点。
        if !self.secure_pending && !matches!(request.action, ForeignAction::Rectangle { .. }) {
            self.keyboard_layer = None;
        }
        match request.action {
            ForeignAction::Activate => {
                self.restore_window(client, surface);
            }
            ForeignAction::Minimize(false) => self.unminimize_window(client, surface),
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
                if let Some(layer) = self
                    .layer_boxes()
                    .iter()
                    .find(|layer| layer.client == request.shell && layer.surface == shell_surface)
                {
                    self.minimize_targets.insert(
                        (client, surface, request.generation),
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

    fn publish_batch(&mut self, geometry: bool) {
        // 批量变化不逐窗口重排；只投影最终状态，排列必须显式给出 normal 窗口的新尺寸。
        self.animation = None;
        for index in 0..self.stack.windows().len() {
            let placed = &self.stack.windows()[index];
            let size = geometry.then_some((placed.width, placed.height));
            self.send_configure(placed.client, placed.surface, size, false);
        }
        if !self.secure_pending {
            self.keyboard_layer = None;
        }
        self.dirty = true;
    }

    fn shell_command(
        &mut self,
        owner: ClientId,
        action: wayland_wire::protocol::liteos_shell_v1::Action,
    ) {
        use wayland_wire::protocol::liteos_shell_v1::Action;
        let active_before = self.stack.active();
        let restoring = action == Action::UndoMinimizeAll
            || (action == Action::ToggleDesktop && self.stack.showing_desktop());
        let changed = match action {
            Action::MinimizeAll => self.stack.minimize_all(owner),
            Action::UndoMinimizeAll => self.stack.undo_minimize_all(owner),
            Action::ToggleDesktop => self.stack.toggle_desktop(owner),
        };
        if changed {
            self.publish_batch(false);
            // 状态先原子生效，沿已有单一过渡 owner 动画化前台窗口；不能逐项执行批量操作。
            // 撤销期间新窗口仍在前台时不对它播放还原动画，它从未属于旧批次。
            let target = if restoring {
                self.stack
                    .active()
                    .filter(|active| Some(*active) != active_before)
            } else {
                active_before
            };
            if let Some((client, surface)) = target {
                self.animate(client, surface, !restoring);
            }
        }
    }

    fn arrange_windows(
        &mut self,
        owner: ClientId,
        layout: Option<wayland_wire::protocol::liteos_shell_v1::Arrangement>,
    ) -> io::Result<()> {
        self.end_grab()?;
        let area = self.work_area();
        let changed = if let Some(layout) = layout {
            let server = &self.server;
            self.stack
                .arrange(owner, layout, area, self.scale as i32, |client, surface| {
                    // 模态窗口禁用其 owner；固定/owned/fullscreen 的筛选仍由窗口栈统一执行。
                    if server.windows().any(|(child_client, window)| {
                        child_client == client && window.modal && window.parent == Some(surface)
                    }) {
                        return None;
                    }
                    server
                        .windows()
                        .find(|(id, window)| *id == client && window.surface == surface)
                        .map(|(_, window)| (window.min_size, window.max_size))
                })
        } else {
            self.stack.undo_arrangement(owner)
        };
        if changed {
            let full = self.full_area();
            self.stack.refit(area, full);
            self.publish_batch(true);
        }
        Ok(())
    }

    /// 最后一个授权对象关闭时先撤销，不能遗留无人能恢复的批量隐藏窗口。
    pub(super) fn sync_shell(&mut self) {
        if let Some(owner) = self.stack.batch_owner()
            && !self.server.has_shell_control(owner)
            && (self.stack.undo_minimize_all(owner) || self.stack.undo_arrangement(owner))
        {
            let (work, full) = (self.work_area(), self.full_area());
            self.stack.refit(work, full);
            self.publish_batch(true);
        }
        use wayland_wire::protocol::liteos_shell_v1::{Arrangement, state};
        let stack = &self.stack;
        self.server.sync_shell(|owner| {
            (u32::from(stack.can_undo(owner)) * state::CANUNDO)
                | (u32::from(stack.batch_owner() == Some(owner) && stack.showing_desktop())
                    * state::SHOWINGDESKTOP)
                | match stack.undo_arrangement_kind(owner) {
                    Some(Arrangement::Cascade) => state::UNDOCASCADE,
                    Some(Arrangement::Horizontal) => state::UNDOHORIZONTAL,
                    Some(Arrangement::Vertical) => state::UNDOVERTICAL,
                    None => 0,
                }
        });
    }

    /// 让外壳看到的窗口列表与当前窗口栈一致。
    pub(super) fn sync_foreign(&mut self) {
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
                    generation: placed.generation,
                    title,
                    app_id,
                    state: ForeignState {
                        maximized: placed.mode == Mode::Maximized,
                        minimized: placed.minimized(),
                        activated: active == Some((placed.client, placed.surface)),
                        fullscreen: placed.mode == Mode::Fullscreen,
                    },
                })
            })
            .collect();
        self.server.sync_foreign(&snapshot);
    }
}

/// 未映射窗口与已映射窗口共用模式选择；最大化请求不能直接打断全屏，撤销全屏也不撤销未全屏窗口的最大化。
fn requested_mode(current: Mode, request: WindowRequest) -> Option<Mode> {
    match request {
        WindowRequest::Maximize(_) if current == Mode::Fullscreen => Some(current),
        WindowRequest::Maximize(on) => Some(if on { Mode::Maximized } else { Mode::Normal }),
        WindowRequest::Fullscreen(false) if current != Mode::Fullscreen => Some(current),
        WindowRequest::Fullscreen(on) => Some(if on { Mode::Fullscreen } else { Mode::Normal }),
        _ => None,
    }
}

/// 此阶段没有 Placement；只保存有效角色的模式意图，首个空提交之后才允许配置，无缓冲时不抢活动窗口。
/// geometry 与协议 flags 是 Stack 模式的短暂投影，不在 Client 保存第二份模式状态。
fn unmapped_request(
    connection: &mut crate::wayland::Client,
    stack: &mut Stack,
    (client, surface, generation): (ClientId, ObjectId, u64),
    request: WindowRequest,
    (work, full): (Geometry, Geometry),
    scale: i32,
) {
    let Some((false, configured)) = connection.toplevel_state(surface, generation) else {
        return;
    };
    let current = stack.prepared_mode(client, surface, generation);
    let mode = if request == WindowRequest::Initialize {
        if configured {
            return;
        }
        current
    } else if let Some(mode) = requested_mode(current, request) {
        stack.prepare_mode(client, surface, generation, mode);
        if !configured {
            return;
        }
        mode
    } else {
        // Minimize 是可忽略提示；移动/缩放/菜单需要已映射窗口及真实按下授权。
        return;
    };
    let area = match mode {
        Mode::Normal => (0, 0, 0, 0),
        Mode::Maximized => work,
        Mode::Fullscreen => full,
    };
    let scale = scale.max(1);
    connection.configure(
        surface,
        area.2 / scale,
        area.3 / scale,
        crate::wayland::ToplevelState {
            maximized: mode == Mode::Maximized,
            fullscreen: mode == Mode::Fullscreen,
            ..crate::wayland::ToplevelState::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::{Client, Output};
    use std::{
        collections::VecDeque,
        io::Write,
        os::fd::{AsRawFd, OwnedFd},
    };
    use wayland_wire::{
        protocol::{
            self, AnyEvent, Interface, wl_compositor, wl_display, wl_registry, wl_shm, wl_shm_pool,
            wl_surface, xdg_surface, xdg_toplevel, xdg_wm_base,
        },
        wire::{DynamicNewId, Framer, Reader, Writer},
    };

    fn id(value: u32) -> ObjectId {
        ObjectId::new(value).unwrap()
    }

    fn send(client: &mut Client, build: impl FnOnce(&mut Writer), fds: Vec<OwnedFd>) {
        let mut wire = Writer::new();
        build(&mut wire);
        client.receive(wire.bytes(), fds).unwrap();
    }

    type Configuration = (i32, i32, Vec<u32>);

    /// 只解码本用例关心的顶层配置和握手序号，消费无关的 registry 与 buffer 事件。
    fn configurations(client: &mut Client) -> (Vec<Configuration>, Vec<u32>) {
        let (bytes, _) = client.output();
        let mut frames = Framer::new();
        frames.push(bytes);
        let length = bytes.len();
        client.consume_output(length);
        let (mut configs, mut serials) = (Vec::new(), Vec::new());
        while let Some(frame) = frames.next_frame().unwrap() {
            let interface = match frame.object.get() {
                22 => Interface::XdgToplevel,
                21 => Interface::XdgSurface,
                _ => continue,
            };
            let mut fds = VecDeque::new();
            let event = protocol::decode_event(
                interface,
                frame.opcode,
                &mut Reader::new(&frame.body, &mut fds),
            )
            .unwrap();
            match event {
                AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure {
                    width,
                    height,
                    states,
                }) => configs.push((
                    width,
                    height,
                    states
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|raw| u32::from_ne_bytes(*raw))
                        .collect(),
                )),
                AnyEvent::XdgSurface(xdg_surface::Event::Configure { serial }) => {
                    serials.push(serial)
                }
                _ => {}
            }
        }
        (configs, serials)
    }

    #[test]
    fn initial_mode_reaches_the_wire_and_transfers_only_to_its_mapped_generation() {
        let output = Output {
            width: 1280,
            height: 800,
            scale: 2,
        };
        let areas = ((0, 0, 1280, 760), (0, 0, 1280, 800));
        let target = (ClientId(1), id(5), 1);
        let mut client = Client::new(output);
        let mut stack = Stack::default();
        send(
            &mut client,
            |wire| {
                wl_display::Request::GetRegistry { registry: id(2) }
                    .encode(ObjectId::DISPLAY, wire)
                    .unwrap();
                for (name, interface, object) in [
                    (1, "wl_compositor", 3),
                    (2, "wl_shm", 4),
                    (3, "xdg_wm_base", 20),
                ] {
                    wl_registry::Request::Bind {
                        name,
                        id: DynamicNewId {
                            interface: interface.into(),
                            version: 1,
                            id: id(object),
                        },
                    }
                    .encode(id(2), wire)
                    .unwrap();
                }
                wl_compositor::Request::CreateSurface { id: id(5) }
                    .encode(id(3), wire)
                    .unwrap();
                xdg_wm_base::Request::GetXdgSurface {
                    id: id(21),
                    surface: id(5),
                }
                .encode(id(20), wire)
                .unwrap();
                xdg_surface::Request::GetToplevel { id: id(22) }
                    .encode(id(21), wire)
                    .unwrap();
                xdg_toplevel::Request::SetMaximized
                    .encode(id(22), wire)
                    .unwrap();
                wl_surface::Request::Commit.encode(id(5), wire).unwrap();
            },
            Vec::new(),
        );
        assert!(
            configurations(&mut client).0.is_empty(),
            "protocol owner must wait for window policy"
        );
        for command in client.take_requests() {
            let WindowCommand::Toplevel(surface, generation, request) = command else {
                panic!("unexpected command")
            };
            unmapped_request(
                &mut client,
                &mut stack,
                (target.0, surface, generation),
                request,
                areas,
                2,
            );
        }
        let (configs, serials) = configurations(&mut client);
        assert_eq!(
            configs,
            [(640, 380, vec![1])],
            "initial maximize uses the work area, not the full output"
        );
        assert_eq!(serials.len(), 1);
        assert!(
            stack.active().is_none(),
            "a window waiting for its first buffer cannot steal activation"
        );
        // configure/ack 与第一幅缓冲之间仍可改模式；同一配置不能被重复 initial commit 覆盖。
        unmapped_request(
            &mut client,
            &mut stack,
            target,
            WindowRequest::Fullscreen(true),
            areas,
            2,
        );
        unmapped_request(
            &mut client,
            &mut stack,
            target,
            WindowRequest::Initialize,
            areas,
            2,
        );
        let (configs, serials) = configurations(&mut client);
        assert_eq!(configs, [(640, 400, vec![2])]);
        let serial = serials[0];
        let path =
            std::env::temp_dir().join(format!("liteos-initial-buffer-{}", std::process::id()));
        let mut file = std::fs::File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        file.write_all(&[0xFF; 16]).unwrap();
        let raw = file.as_raw_fd();
        send(
            &mut client,
            |wire| {
                xdg_surface::Request::AckConfigure { serial }
                    .encode(id(21), wire)
                    .unwrap();
                wire.begin(id(4), 0);
                wire.object(Some(id(10)));
                wire.fd(raw);
                wire.int(16);
                wire.end().unwrap();
                wl_shm_pool::Request::CreateBuffer {
                    id: id(11),
                    offset: 0,
                    width: 2,
                    height: 2,
                    stride: 8,
                    format: wl_shm::Format::Argb8888,
                }
                .encode(id(10), wire)
                .unwrap();
                wl_surface::Request::Attach {
                    buffer: Some(id(11)),
                    x: 0,
                    y: 0,
                }
                .encode(id(5), wire)
                .unwrap();
                wl_surface::Request::Commit.encode(id(5), wire).unwrap();
            },
            vec![file.into()],
        );
        let mapped: Vec<_> = client
            .windows()
            .map(|window| {
                (
                    target.0,
                    window.surface,
                    window.generation,
                    4,
                    4,
                    Initial::Cascade,
                )
            })
            .collect();
        assert!(stack.sync(&mapped, (1280, 800), 2));
        stack.refit(areas.0, areas.1);
        let placed = stack.placement(target.0, target.1).unwrap();
        assert_eq!(
            (placed.mode, placed.x, placed.y, placed.width, placed.height),
            (Mode::Fullscreen, 0, 0, 1280, 800)
        );
        assert_eq!(
            stack.prepared_mode(target.0, target.1, target.2),
            Mode::Normal,
            "mode has moved into Placement"
        );
        assert_eq!(
            stack.set_mode(target.0, target.1, Mode::Normal, areas.0, areas.1),
            Some((0, 0)),
            "initial fullscreen has no invented normal-size restore record"
        );
        configurations(&mut client);
        send(
            &mut client,
            |wire| {
                wl_surface::Request::Attach {
                    buffer: None,
                    x: 0,
                    y: 0,
                }
                .encode(id(5), wire)
                .unwrap();
                wl_surface::Request::Commit.encode(id(5), wire).unwrap();
            },
            Vec::new(),
        );
        stack.sync(&[], (1280, 800), 2);
        unmapped_request(
            &mut client,
            &mut stack,
            target,
            WindowRequest::Maximize(true),
            areas,
            2,
        );
        assert!(
            configurations(&mut client).0.is_empty(),
            "old generation is inert even while a new mapping is being prepared"
        );
        send(
            &mut client,
            |wire| wl_surface::Request::Commit.encode(id(5), wire).unwrap(),
            Vec::new(),
        );
        for command in client.take_requests() {
            if let WindowCommand::Toplevel(surface, generation, request) = command {
                unmapped_request(
                    &mut client,
                    &mut stack,
                    (target.0, surface, generation),
                    request,
                    areas,
                    2,
                );
            }
        }
        assert_eq!(configurations(&mut client).0, [(0, 0, Vec::new())]);
    }
}
