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
    desktop::Stack,
    input::{Event, Input},
    scanout::Scanout,
    wayland::{ClientId, Server},
};

/// Linux `BTN_LEFT` 与键盘键码（`linux/input-event-codes.h`）。
const BUTTON_LEFT: u16 = 0x110;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_F4: u16 = 62;

pub struct Session {
    pub scanout: Scanout,
    pub input: Input,
    pub server: Server,
    pub theme: Theme,
    /// 输出的整数 scale。
    pub scale: u32,
    /// 硬件光标的图像缓冲；释放它会撤掉光标图像，所以要在整个会话期间持有。
    pub _cursor: DumbBuffer,
}

/// 窗口内容放大到输出 scale 所需的整数倍；客户端 scale 大于输出时不缩小，按 1 处理。
fn factor(output_scale: u32, surface_scale: i32) -> i32 {
    (output_scale as i32 / surface_scale.max(1)).max(1)
}

impl Session {
    fn now_ms(started: Instant) -> u32 {
        started.elapsed().as_millis() as u32
    }

    /// 运行到出错为止；正常情况下不返回。
    ///
    /// # Errors
    ///
    /// DRM、输入或监听 socket 的不可恢复错误。
    pub fn run(mut self) -> io::Result<()> {
        let started = Instant::now();
        let mut stack = Stack::default();
        let screen = (self.scanout.width() as i32, self.scanout.height() as i32);
        let mut events = Vec::new();
        // 一次性标记：运行时门禁据此确认输入路径端到端可用；缺少它，门禁只能靠截图间接判断指针是否被读到。
        let (mut saw_pointer, mut saw_key) = (false, false);
        // 第一个窗口出现之前一直显示启动画面（登录程序会接管之后的画面）；缺少它，空桌面会盖掉内核交接的画面。
        let mut desktop_shown = false;
        let mut dirty = false;
        let mut commits_seen = 0;
        // 已通知客户端"已激活"的窗口；与栈顶不同时发送 configure。缺少它，客户端永远不知道自己是否活动。
        let mut notified = None;
        let mut alt_down = false;
        loop {
            if dirty && desktop_shown && !self.scanout.flip_pending() {
                self.present(&stack)?;
                dirty = false;
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
                self.server.finish_frame(Self::now_ms(started));
            }
            for index in input_ready {
                self.input.drain(index, &mut events)?;
            }
            for event in events.drain(..) {
                match event {
                    Event::PointerMoved { x, y } => {
                        self.scanout.move_cursor((x, y))?;
                        if !std::mem::replace(&mut saw_pointer, true) {
                            println!("compositor: first pointer event {x},{y}");
                        }
                    }
                    Event::Button {
                        code: BUTTON_LEFT,
                        pressed: true,
                    } => {
                        // 单击窗口：激活并置顶。
                        let (x, y) = self.input.pointer().position();
                        if let Some(target) = stack
                            .window_at(x, y)
                            .map(|placed| (placed.client, placed.surface))
                            && stack.raise(target.0, target.1)
                        {
                            dirty = true;
                        }
                    }
                    Event::Button { .. } => {}
                    Event::Key { code, pressed } => {
                        if !std::mem::replace(&mut saw_key, true) {
                            println!("compositor: first key event");
                        }
                        match code {
                            KEY_LEFTALT | KEY_RIGHTALT => alt_down = pressed,
                            // Alt+F4：请求关闭活动窗口；客户端可以拒绝或先询问用户。
                            KEY_F4 if pressed && alt_down => {
                                if let Some((client, surface)) = stack.active() {
                                    self.server
                                        .with_client(client, |client| client.close(surface));
                                    println!("compositor: close requested");
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            self.server.dispatch(&server_events)?;

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
            let before = stack.windows().len();
            if stack.sync(&mapped, screen, self.scale as i32) {
                dirty = true;
                if stack.windows().len() != before {
                    println!("compositor: windows={}", stack.windows().len());
                    for (_, window) in self.server.windows() {
                        println!(
                            "compositor: window {:?} app_id={:?}",
                            window.title, window.app_id
                        );
                    }
                }
            }
            desktop_shown |= !stack.windows().is_empty();
            if stack.active() != notified {
                if let Some((client, surface)) = notified {
                    self.server
                        .with_client(client, |client| client.configure(surface, 0, 0, false));
                }
                if let Some((client, surface)) = stack.active() {
                    self.server
                        .with_client(client, |client| client.configure(surface, 0, 0, true));
                }
                notified = stack.active();
            }
            let commits = self.server.commits();
            if commits != commits_seen {
                commits_seen = commits;
                dirty = true;
            }
            // 交互式移动、缩放、最大化等窗口请求尚未实现；先取走丢弃，避免恶意客户端让队列无限增长。
            drop(self.server.take_requests());
        }
    }

    fn present(&mut self, stack: &Stack) -> io::Result<()> {
        let contents: HashMap<(ClientId, ObjectId), (&crate::wayland::Content, i32)> = self
            .server
            .windows()
            .map(|(client, window)| ((client, window.surface), (window.content, window.scale)))
            .collect();
        let drawables: Vec<Drawable<'_>> = stack
            .windows()
            .iter()
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
