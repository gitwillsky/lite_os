//! 会话画面投影与叠加层素材；只借用 Session 的窗口与模式状态，不复制可写状态。

use super::*;

impl Session {
    /// 首次需要覆盖层时加载字体与图标；失败只记一行日志，覆盖层随之不显示。
    pub(super) fn ensure_art(&mut self) {
        if self.switcher_art.is_none() {
            match switcher::Art::load(self.scale) {
                Ok(art) => self.switcher_art = Some(art),
                Err(error) => println!("compositor: overlay art unavailable: {error}"),
            }
        }
    }

    /// 将已提交的 surface、窗口位置与外壳叠加层投影到下一块输出缓冲。
    ///
    /// # Errors
    /// 输出绘制或 DRM page flip 请求失败。
    pub(super) fn present(&mut self) -> io::Result<()> {
        let boxes = self.layer_boxes();
        let popup_views = self.server.popups();
        let server = &self.server;
        let output = self.scale as i32;
        let screen = canvas::Rect::new(
            0,
            0,
            self.scanout.width() as i32,
            self.scanout.height() as i32,
        );
        let mut drawables = Vec::new();
        let mut visible = Vec::new();
        let mut append = |client, surface, x: i32, y: i32, transform: Option<canvas::Rect>| {
            let tree = server.subtree(client, surface);
            let Some(root) = tree.iter().find(|leaf| leaf.surface == surface) else {
                return;
            };
            let source = Drawable::surface(x, y, root.content, output).rect;
            for leaf in tree {
                let mut drawable = Drawable::surface(
                    x.saturating_add(leaf.x.saturating_mul(output)),
                    y.saturating_add(leaf.y.saturating_mul(output)),
                    leaf.content,
                    output,
                );
                if let Some(target) = transform {
                    drawable = drawable.transformed(source, target);
                }
                if drawable.rect.intersect(screen).is_empty() {
                    continue;
                }
                drawables.push(drawable);
                visible.push((client, leaf.surface));
            }
        };
        for layer in boxes.iter().filter(|layer| layer.layer < Layer::Top) {
            append(layer.client, layer.surface, layer.x, layer.y, None);
        }
        for placed in self.stack.windows().iter().filter(|placed| {
            !placed.minimized
                && !self.animation.as_ref().is_some_and(|animation| {
                    animation.client == placed.client && animation.surface == placed.surface
                })
        }) {
            append(placed.client, placed.surface, placed.x, placed.y, None);
        }
        if let Some(animation) = &self.animation
            && let Some(placed) = self.stack.placement(animation.client, animation.surface)
        {
            let (x, y, width, height) = animation.frame();
            append(
                placed.client,
                placed.surface,
                placed.x,
                placed.y,
                Some(canvas::Rect::new(x, y, width, height)),
            );
        }
        for (client, popup) in &popup_views {
            if let Some(root) = self
                .stack
                .placement(*client, popup.client_root)
                .filter(|root| !root.minimized)
            {
                append(
                    *client,
                    popup.surface,
                    root.x.saturating_add(popup.x.saturating_mul(output)),
                    root.y.saturating_add(popup.y.saturating_mul(output)),
                    None,
                );
            }
        }
        for layer in boxes.iter().filter(|layer| layer.layer >= Layer::Top) {
            append(layer.client, layer.surface, layer.x, layer.y, None);
        }
        let (theme, scale) = (&self.theme, self.scale as i32);
        let (art, switcher, menu) = (
            self.switcher_art.as_ref(),
            self.switcher.as_ref(),
            self.window_menu.as_ref(),
        );
        let screen_size = (screen.width, screen.height);
        // 叠加层只占面板/菜单的矩形；GPU 只上传并绘制这一块，不为它准备整屏纹理。
        let overlay_bounds = art.and_then(|_| {
            [
                switcher.map(|switcher| switcher::bounds(screen_size, scale, switcher)),
                menu.map(window_menu::WindowMenu::rect),
            ]
            .into_iter()
            .flatten()
            .reduce(|a, b| {
                let (x, y) = (a.x.min(b.x), a.y.min(b.y));
                canvas::Rect::new(
                    x,
                    y,
                    a.right().max(b.right()) - x,
                    a.bottom().max(b.bottom()) - y,
                )
            })
            .map(|bounds| bounds.intersect(screen))
        });
        let overlay = |mut surface: canvas::Surface<'_>| {
            let (Some(art), Some(bounds)) = (art, overlay_bounds) else {
                return;
            };
            let screen = (surface.width(), surface.height());
            surface.set_clip(bounds);
            let mut painter = classic_ui::Painter::new(surface, theme, &art.fonts, scale);
            if let Some(switcher) = switcher {
                switcher::paint(&mut painter, art, screen, switcher);
            }
            if let Some(menu) = menu {
                menu.paint(&mut painter);
            }
        };
        self.scanout
            .compose(&self.theme, &drawables, overlay_bounds, overlay)?;
        // 硬件光标不在场景四边形中；实际上传的客户端光标仍参与同一显示节拍，避免动画光标等待 frame 永久停住。
        if self.applied_client_cursor.is_some()
            && let Shape::Client {
                client, surface, ..
            } = self.shape
        {
            visible.push((client, surface));
        }
        self.server.stage_frame(&visible);
        self.scanout.request_flip()
    }
}
