//! 对账已提交映射并按连接线序裁决窗口与外壳事务；窗口栈是批量成员和桌面状态的唯一 owner。

use super::*;

impl Session {
    /// 与客户端的已映射窗口对账；窗口增减、尺寸变化与新的 commit 都会触发重绘。
    pub(super) fn sync_windows(&mut self) -> io::Result<bool> {
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
                continue;
            };
            match request {
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
