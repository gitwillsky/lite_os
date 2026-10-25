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
        let layer_images: HashMap<(ClientId, ObjectId), (&crate::wayland::Content, i32)> = self
            .server
            .layers()
            .map(|(client, layer)| ((client, layer.surface), (layer.content, layer.scale)))
            .collect();
        let window_images: HashMap<
            (ClientId, ObjectId),
            (
                &crate::wayland::Content,
                i32,
                Vec<crate::wayland::ChildView<'_>>,
            ),
        > = self
            .server
            .windows()
            .map(|(client, window)| {
                (
                    (client, window.surface),
                    (window.content, window.scale, window.children),
                )
            })
            .collect();
        let popup_views = self.server.popups();
        let output_scale = self.scale;
        let layer_drawable = |layer: &LayerBox| {
            let (content, scale) = layer_images.get(&(layer.client, layer.surface))?;
            Some(Drawable {
                x: layer.x,
                y: layer.y,
                content,
                factor: factor(output_scale, *scale),
            })
        };
        let below = boxes
            .iter()
            .filter(|layer| layer.layer < Layer::Top)
            .filter_map(layer_drawable);
        let windows = self
            .stack
            .windows()
            .iter()
            .filter(|placed| !placed.minimized)
            .filter_map(|placed| {
                let (content, scale, children) =
                    window_images.get(&(placed.client, placed.surface))?;
                // 窗口本体先画，子 surface 按位置压在它上面。
                let body = Drawable {
                    x: placed.x,
                    y: placed.y,
                    content,
                    factor: factor(output_scale, *scale),
                };
                let output = output_scale as i32;
                let subsurfaces = children.iter().map(move |child| Drawable {
                    x: placed.x + child.x * output,
                    y: placed.y + child.y * output,
                    content: child.content,
                    factor: factor(output_scale, child.scale),
                });
                Some(std::iter::once(body).chain(subsurfaces))
            })
            .flatten();
        let popups: Vec<Drawable<'_>> = popup_views
            .iter()
            .filter_map(|(client, popup)| {
                let root = self.stack.placement(*client, popup.client_root)?;
                if root.minimized {
                    return None;
                }
                let output = output_scale as i32;
                let (x, y) = (root.x + popup.x * output, root.y + popup.y * output);
                let body = Drawable {
                    x,
                    y,
                    content: popup.content,
                    factor: factor(output_scale, popup.scale),
                };
                Some(
                    std::iter::once(body).chain(popup.children.iter().map(move |child| Drawable {
                        x: x + child.x * output,
                        y: y + child.y * output,
                        content: child.content,
                        factor: factor(output_scale, child.scale),
                    })),
                )
            })
            .flatten()
            .collect();
        let above = boxes
            .iter()
            .filter(|layer| layer.layer >= Layer::Top)
            .filter_map(layer_drawable);
        let drawables: Vec<Drawable<'_>> =
            below.chain(windows).chain(popups).chain(above).collect();
        let (theme, scale) = (&self.theme, self.scale as i32);
        let (art, switcher, menu) = (
            self.switcher_art.as_ref(),
            self.switcher.as_ref(),
            self.window_menu.as_ref(),
        );
        let frame = self.animation.as_ref().map(Animation::frame);
        let inversions: Vec<_> = frame
            .into_iter()
            .flat_map(|(x, y, width, height)| {
                let thickness = 2 * scale;
                [
                    canvas::Rect::new(x, y, width, thickness),
                    canvas::Rect::new(x, y + height - thickness, width, thickness),
                    canvas::Rect::new(x, y + thickness, thickness, height - 2 * thickness),
                    canvas::Rect::new(
                        x + width - thickness,
                        y + thickness,
                        thickness,
                        height - 2 * thickness,
                    ),
                ]
            })
            .collect();
        let has_overlay = art.is_some() && (switcher.is_some() || menu.is_some());
        let overlay = |surface: canvas::Surface<'_>| {
            let Some(art) = art else { return };
            let screen = (surface.width(), surface.height());
            let mut painter = classic_ui::Painter::new(surface, theme, &art.typeface, scale);
            if let Some(switcher) = switcher {
                switcher::paint(&mut painter, art, screen, switcher);
            }
            if let Some(menu) = menu {
                menu.paint(&mut painter);
            }
        };
        self.scanout
            .compose(&self.theme, &drawables, &inversions, has_overlay, overlay)?;
        self.scanout.request_flip()
    }
}
