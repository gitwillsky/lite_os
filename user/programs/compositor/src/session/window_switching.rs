//! 最小化/还原动画与 Alt+Tab 切换面板；窗口栈与面板状态仍由 Session 唯一持有。

use super::*;

impl Session {
    /// 开始最小化（`minimizing`）或还原的动画：窗口矩形与任务按钮矩形之间。没有任务按钮位置时取屏幕底边中部。
    pub(super) fn animate(&mut self, client: ClientId, surface: ObjectId, minimizing: bool) {
        let Some(placement) = self.stack.placement(client, surface).copied() else {
            return;
        };
        let scale = self.scale as i32;
        let window = (placement.x, placement.y, placement.width, placement.height);
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
        self.start_animation(client, surface, from, to);
    }

    /// 播放整窗几何过渡；同一窗口反向操作从当前画面继续，避免跳回端点。输入状态由操作立即提交，动画不延迟激活。
    pub(super) fn start_animation(
        &mut self,
        client: ClientId,
        surface: ObjectId,
        from: (i32, i32, i32, i32),
        to: (i32, i32, i32, i32),
    ) {
        let previous = self.animation.take();
        let current = previous
            .as_ref()
            .filter(|animation| animation.client == client && animation.surface == surface)
            .map(Animation::frame);
        self.animation = Some(Animation {
            client,
            surface,
            from: current.unwrap_or(from),
            to,
            start: Instant::now(),
        });
        self.dirty = true;
    }

    /// 最小化窗口并播放动画。
    pub(super) fn minimize_window(&mut self, client: ClientId, surface: ObjectId) {
        if self
            .stack
            .placement(client, surface)
            .is_some_and(|placed| !placed.minimized())
        {
            self.animate(client, surface, true);
        }
        if self.stack.minimize(client, surface) {
            // 最小化不一定改变键盘焦点（例如从任务栏最小化后台窗口），所以由这里通知 suspended；
            // 还原总会激活窗口，由焦点同步的 configure 带上解除后的状态。
            self.send_configure(client, surface, None, false);
            self.dirty = true;
        }
    }

    /// 立即还原并激活窗口，画面独立播放整窗过渡；重复激活不重启动画或丢弃紧接着的键盘输入。
    pub(super) fn restore_window(&mut self, client: ClientId, surface: ObjectId) {
        self.unminimize_window(client, surface);
        self.dirty |= self.stack.raise(client, surface);
    }

    /// 仅恢复可见性，保留已有 Z 序；批量撤销不能按协议句柄的枚举次序重新排列窗口。
    pub(super) fn unminimize_window(&mut self, client: ClientId, surface: ObjectId) {
        if self.stack.unminimize(client, surface) {
            self.animate(client, surface, false);
            self.send_configure(client, surface, None, false);
            self.dirty = true;
        }
    }

    /// 以最近使用顺序（栈顶在前）列出全部窗口并打开切换面板。
    pub(super) fn open_switcher(&mut self, reverse: bool) {
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
    pub(super) fn commit_switcher(&mut self) {
        if let Some(switcher) = self.switcher.take() {
            let (client, surface) = switcher.target();
            self.restore_window(client, surface);
        }
    }
}
