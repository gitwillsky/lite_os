//! 窗口系统菜单：打开、关闭、点击命中与命令执行；菜单状态仍由 Session 唯一持有。

use super::*;

impl Session {
    /// 在物理坐标 `at` 打开窗口菜单，并让窗口看到指针离开。
    pub(super) fn open_menu(
        &mut self,
        placement: crate::desktop::Placement,
        at: (i32, i32),
        cues: bool,
    ) {
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
    pub(super) fn close_menu(&mut self) {
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
    pub(super) fn menu_click(&mut self) -> io::Result<()> {
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

    pub(super) fn run_menu_command(
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
}
