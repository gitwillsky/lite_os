//! 显示会话主循环：输入、Wayland 连接、窗口栈与整帧合成的唯一装配点。

use std::{collections::HashMap, io, os::fd::AsFd, time::Instant};

use classic_theme::Theme;
use linux_uapi::{
    drm::DumbBuffer,
    unix::{PollEvents, PollFd, poll},
};
use wayland_wire::wire::ObjectId;

use crate::{
    compose::{self, Drawable},
    desktop::{Mode, Stack},
    focus::{Seat, Shortcut, window_configure},
    input::{Event, Input},
    interaction::{Edges, Grab, Kind},
    scanout::Scanout,
    wayland::{ClientId, Server, WindowRequest},
};

/// Linux `BTN_LEFT`（`linux/input-event-codes.h`）。
const BUTTON_LEFT: u16 = 0x110;

/// 窗口内容放大到输出 scale 所需的整数倍；客户端 scale 大于输出时不缩小，按 1 处理。
fn factor(output_scale: u32, surface_scale: i32) -> i32 {
    (output_scale as i32 / surface_scale.max(1)).max(1)
}

/// 进行中的指针抓取，以及最近一次发给客户端的缩放目标（尺寸没变就不重复发 configure）。
struct ActiveGrab {
    grab: Grab,
    last_size: Option<(i32, i32)>,
}

pub struct Session {
    scanout: Scanout,
    input: Input,
    server: Server,
    theme: Theme,
    /// 输出的整数 scale。
    scale: u32,
    /// 硬件光标的图像缓冲；释放它会撤掉光标图像，所以要在整个会话期间持有。
    _cursor: DumbBuffer,
    stack: Stack,
    seat: Seat,
    grab: Option<ActiveGrab>,
    /// 第一个窗口出现之前一直显示启动画面（登录程序会接管之后的画面）；缺少它，空桌面会盖掉内核交接的画面。
    desktop_shown: bool,
    dirty: bool,
    commits_seen: u64,
    /// 一次性标记：运行时门禁据此确认输入路径端到端可用；缺少它，门禁只能靠截图间接判断指针是否被读到。
    saw_pointer: bool,
    saw_key: bool,
    started: Instant,
}

impl Session {
    pub fn new(
        scanout: Scanout,
        input: Input,
        server: Server,
        theme: Theme,
        scale: u32,
        cursor: DumbBuffer,
    ) -> Self {
        Self {
            scanout,
            input,
            server,
            theme,
            scale,
            _cursor: cursor,
            stack: Stack::default(),
            seat: Seat::default(),
            grab: None,
            desktop_shown: false,
            dirty: false,
            commits_seen: 0,
            saw_pointer: false,
            saw_key: false,
            started: Instant::now(),
        }
    }

    fn now_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn screen(&self) -> (i32, i32) {
        (self.scanout.width() as i32, self.scanout.height() as i32)
    }

    /// 运行到出错为止；正常情况下不返回。
    ///
    /// # Errors
    ///
    /// DRM、输入或监听 socket 的不可恢复错误。
    pub fn run(mut self) -> io::Result<()> {
        let mut events = Vec::new();
        loop {
            if self.dirty && self.desktop_shown && !self.scanout.flip_pending() {
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
                poll(&mut descriptors, None)?;
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
            for event in events.drain(..) {
                self.on_input(event)?;
            }
            self.server.dispatch(&server_events)?;
            self.sync_windows();
            self.handle_requests();
            self.sync_focus();
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
                if self.grab.is_some() {
                    self.drag((x, y));
                } else {
                    self.seat.update_pointer(
                        &mut self.server,
                        &self.stack,
                        (x, y),
                        scale,
                        time,
                        true,
                    );
                }
            }
            Event::Button { code, pressed } => {
                if code == BUTTON_LEFT && !pressed && self.grab.is_some() {
                    self.end_grab();
                } else if code == BUTTON_LEFT && pressed {
                    // 单击窗口：激活并置顶。
                    let (x, y) = self.input.pointer().position();
                    if let Some(target) = self
                        .stack
                        .window_at(x, y)
                        .map(|placed| (placed.client, placed.surface))
                        && self.stack.raise(target.0, target.1)
                    {
                        self.dirty = true;
                    }
                }
                self.seat.button(&mut self.server, code, pressed, time);
            }
            Event::Key { code, pressed } => {
                if !std::mem::replace(&mut self.saw_key, true) {
                    println!("compositor: first key event");
                }
                // 键盘焦点必须先于这次按键同步：刚激活的窗口应该收到触发它之后的第一个键。
                self.seat
                    .sync_keyboard(&mut self.server, &self.stack, scale);
                let shortcut = self.seat.key(&mut self.server, code, pressed, time);
                self.on_shortcut(shortcut);
            }
        }
        Ok(())
    }

    fn on_shortcut(&mut self, shortcut: Shortcut) {
        match shortcut {
            Shortcut::None => {}
            Shortcut::CloseActive => {
                if let Some((client, surface)) = self.stack.active() {
                    self.server
                        .with_client(client, |client| client.close(surface));
                    println!("compositor: close requested");
                }
            }
            Shortcut::CycleWindows => {
                if let Some((client, surface)) = self.stack.cycle_target() {
                    self.dirty |= self.stack.raise(client, surface);
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
    fn sync_windows(&mut self) {
        let mapped: Vec<(ClientId, ObjectId, i32, i32)> = self
            .server
            .windows()
            .map(|(client, window)| {
                let factor = factor(self.scale, window.scale);
                (
                    client,
                    window.surface,
                    window.content.width as i32 * factor,
                    window.content.height as i32 * factor,
                )
            })
            .collect();
        let before = self.stack.windows().len();
        if self.stack.sync(&mapped, self.screen(), self.scale as i32) {
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
        self.desktop_shown |= !self.stack.windows().is_empty();
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
            self.grab = None;
            self.seat.end_grab();
        }
    }

    /// 键盘焦点跟随活动窗口；窗口布局变化会改变光标下的窗口，位置没动所以不发 motion。
    fn sync_focus(&mut self) {
        let scale = self.scale as i32;
        self.seat
            .sync_keyboard(&mut self.server, &self.stack, scale);
        let time = self.now_ms();
        let pointer = self.input.pointer().position();
        self.seat
            .update_pointer(&mut self.server, &self.stack, pointer, scale, time, false);
    }

    /// 处理客户端的窗口请求：交互式移动/缩放、最大化、全屏与最小化。
    fn handle_requests(&mut self) {
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
                        });
                    }
                }
                WindowRequest::Maximize(on) => self.set_mode(
                    client,
                    surface,
                    if on { Mode::Maximized } else { Mode::Normal },
                ),
                WindowRequest::Fullscreen(on) => self.set_mode(
                    client,
                    surface,
                    if on { Mode::Fullscreen } else { Mode::Normal },
                ),
                WindowRequest::Minimize => self.dirty |= self.stack.minimize(client, surface),
            }
        }
    }

    fn set_mode(&mut self, client: ClientId, surface: ObjectId, mode: Mode) {
        if let Some(size) = self.stack.set_mode(client, surface, mode, self.screen()) {
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

    fn end_grab(&mut self) {
        let Some(active) = self.grab.take() else {
            return;
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
    }

    fn present(&mut self) -> io::Result<()> {
        let contents: HashMap<(ClientId, ObjectId), (&crate::wayland::Content, i32)> = self
            .server
            .windows()
            .map(|(client, window)| ((client, window.surface), (window.content, window.scale)))
            .collect();
        let drawables: Vec<Drawable<'_>> = self
            .stack
            .windows()
            .iter()
            .filter(|placement| !placement.minimized)
            .filter_map(|placement| {
                let (content, scale) = contents.get(&(placement.client, placement.surface))?;
                Some(Drawable {
                    placement: *placement,
                    content,
                    factor: factor(self.scale, *scale),
                })
            })
            .collect();
        if !compose::compose(self.scanout.back(), &self.theme, &drawables) {
            return Err(io::Error::other(
                "scanout buffer cannot back a drawing surface",
            ));
        }
        self.scanout.request_flip()
    }
}
