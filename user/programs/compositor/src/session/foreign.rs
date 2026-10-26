//! 外壳经 foreign-toplevel 发出的窗口请求与窗口快照同步；协议状态由 wayland 层持有。

use super::*;

impl Session {
    /// 处理外壳（任务栏）通过 foreign-toplevel 对窗口提出的操作。
    pub(super) fn handle_foreign_requests(&mut self) {
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
}
