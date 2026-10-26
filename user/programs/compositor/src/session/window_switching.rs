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
        // 已有还原被其他窗口操作打断时先完成它；同一窗口反向操作从当前画面继续，避免位置跳回端点。
        let previous = self.animation.take();
        let current = previous
            .as_ref()
            .filter(|animation| animation.client == client && animation.surface == surface)
            .map(Animation::frame);
        if let Some(animation) =
            previous.filter(|animation| animation.client != client || animation.surface != surface)
            && animation.restoring
        {
            self.stack.raise(animation.client, animation.surface);
        }
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
            client,
            surface,
            from: current.unwrap_or(from),
            to,
            start: Instant::now(),
            restoring: !minimizing,
        });
        self.dirty = true;
    }

    /// 最小化窗口并播放动画。
    pub(super) fn minimize_window(&mut self, client: ClientId, surface: ObjectId) {
        if self.stack.placement(client, surface).is_some_and(|placed| {
            !placed.minimized
                || self.animation.as_ref().is_some_and(|animation| {
                    animation.client == client
                        && animation.surface == surface
                        && animation.restoring
                })
        }) {
            self.animate(client, surface, true);
        }
        self.dirty |= self.stack.minimize(client, surface);
    }

    /// 激活窗口；最小化窗口先显示整窗还原动画，结束时提升输入栈；重复激活不重启动画。
    pub(super) fn restore_window(&mut self, client: ClientId, surface: ObjectId) {
        if self
            .stack
            .placement(client, surface)
            .is_some_and(|placed| placed.minimized)
        {
            if self.animation.as_ref().is_some_and(|animation| {
                animation.client == client && animation.surface == surface && animation.restoring
            }) {
                return;
            }
            self.animate(client, surface, false);
            return;
        }
        self.dirty |= self.stack.raise(client, surface);
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
