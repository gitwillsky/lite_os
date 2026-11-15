//! 按已提交 surface 树的绘制逆序做输入命中；窗口激活与指针焦点使用各自正确的目标。
use super::*;
use crate::desktop::Placement;

impl Session {
    /// 返回子 surface 对应的窗口/外壳根；popup 的键盘与窗口管理目标仍为其 xdg_toplevel。
    pub(super) fn root_target(&self, target: (ClientId, ObjectId)) -> Option<(ClientId, ObjectId)> {
        let root = self.server.root_surface(target.0, target.1)?;
        if self.stack.placement(target.0, root).is_some() {
            return Some((target.0, root));
        }
        let root = self
            .server
            .popups()
            .into_iter()
            .find(|(client, popup)| *client == target.0 && popup.surface == root)
            .map_or(root, |(_, popup)| popup.client_root);
        Some((target.0, root))
    }

    /// 将协议的光标矩形换算成输出像素；放不下时改到光标上方。
    pub(super) fn ime_popup_box(
        &self,
        boxes: &[LayerBox],
    ) -> Option<((ClientId, ObjectId), (i32, i32))> {
        let (popup, field, caret, (width, height)) = self.server.input_popup()?;
        let origin = self.locate_target(boxes, field)?.origin;
        let scale = self.scale as i32;
        let (screen_width, screen_height) = self.screen();
        let x = origin.0.saturating_add(caret.x.saturating_mul(scale));
        let y = origin.1.saturating_add(caret.y.saturating_mul(scale));
        let below = y.saturating_add(caret.height.saturating_mul(scale));
        let top = if below.saturating_add(height) <= screen_height {
            below
        } else {
            y.saturating_sub(height)
        };
        Some((
            popup,
            (
                x.clamp(0, (screen_width - width).max(0)),
                top.clamp(0, (screen_height - height).max(0)),
            ),
        ))
    }

    /// 隐式抓取跟随真实 surface：窗口/外壳/popup 移动后仍向原来的子 surface 发送坐标。
    pub(super) fn locate_target(
        &self,
        boxes: &[LayerBox],
        target: (ClientId, ObjectId),
    ) -> Option<Hit> {
        let root = self.server.root_surface(target.0, target.1)?;
        let scale = self.scale as i32;
        let popup = self
            .server
            .input_popup()
            .filter(|(popup, _, _, _)| *popup == (target.0, root));
        let (origin, generation) = popup
            .and_then(|_| self.ime_popup_box(boxes))
            .map(|(_, origin)| (origin, None))
            .or_else(|| {
                boxes
                    .iter()
                    .find(|layer| (layer.client, layer.surface) == (target.0, root))
                    .map(|layer| ((layer.x, layer.y), None))
            })
            .or_else(|| {
                self.stack
                    .placement(target.0, root)
                    .filter(|placed| self.stack.visible(placed))
                    .map(|placed| ((placed.x, placed.y), Some(placed.generation)))
            })
            .or_else(|| {
                self.server
                    .popups()
                    .into_iter()
                    .find_map(|(client, popup)| {
                        if client != target.0 || popup.surface != root {
                            return None;
                        }
                        let placed = self
                            .stack
                            .placement(client, popup.client_root)
                            .filter(|placed| self.stack.visible(placed))?;
                        Some((
                            (
                                placed.x.saturating_add(popup.x.saturating_mul(scale)),
                                placed.y.saturating_add(popup.y.saturating_mul(scale)),
                            ),
                            Some(placed.generation),
                        ))
                    })
            })?;
        self.server
            .subtree(target.0, root)
            .into_iter()
            .find(|leaf| leaf.surface == target.1)
            .map(|leaf| Hit {
                target,
                generation,
                origin: (
                    origin.0.saturating_add(leaf.x.saturating_mul(scale)),
                    origin.1.saturating_add(leaf.y.saturating_mul(scale)),
                ),
            })
    }

    fn tree_hit(
        &self,
        client: ClientId,
        surface: ObjectId,
        origin: (i32, i32),
        point: (i32, i32),
        generation: Option<u64>,
    ) -> Option<Hit> {
        let scale = self.scale as i32;
        self.server
            .subtree(client, surface)
            .into_iter()
            .rev()
            .find_map(|leaf| {
                let (x, y) = (
                    origin.0.saturating_add(leaf.x.saturating_mul(scale)),
                    origin.1.saturating_add(leaf.y.saturating_mul(scale)),
                );
                let rect = Drawable::surface(x, y, leaf.content, scale).rect;
                if !(rect.x..rect.right()).contains(&point.0)
                    || !(rect.y..rect.bottom()).contains(&point.1)
                {
                    return None;
                }
                self.server
                    .accepts_input(
                        client,
                        leaf.surface,
                        (f64::from(point.0) - f64::from(x)) / f64::from(scale),
                        (f64::from(point.1) - f64::from(y)) / f64::from(scale),
                    )
                    .then_some(Hit {
                        target: (client, leaf.surface),
                        generation,
                        origin: (x, y),
                    })
            })
    }

    /// 命中指定窗口组 `windows` 所属的最上层 popup；没有包含 point 的输入区域时返回 None。
    pub(super) fn popup_hit(&self, point: (i32, i32), windows: &[Placement]) -> Option<Hit> {
        let scale = self.scale as i32;
        self.server
            .popups()
            .into_iter()
            .rev()
            .find_map(|(client, popup)| {
                let root = windows.iter().find(|root| {
                    root.client == client
                        && root.surface == popup.client_root
                        && self.stack.visible(root)
                })?;
                self.tree_hit(
                    client,
                    popup.surface,
                    (
                        root.x.saturating_add(popup.x.saturating_mul(scale)),
                        root.y.saturating_add(popup.y.saturating_mul(scale)),
                    ),
                    point,
                    Some(root.generation),
                )
            })
    }

    /// 输入与绘制按同一棵已提交树的逆序命中，input region 空洞允许点击穿透。
    pub(super) fn hit_test(&self, boxes: &[LayerBox], point: (i32, i32)) -> Option<Hit> {
        if self.secure_pending {
            return None;
        }
        let layer_hit = |layers: std::ops::RangeInclusive<Layer>| {
            boxes
                .iter()
                .rev()
                .filter(|layer| layers.contains(&layer.layer))
                .find_map(|layer| {
                    self.tree_hit(layer.client, layer.surface, (layer.x, layer.y), point, None)
                })
        };
        let window_hit = |windows: &[Placement]| {
            windows
                .iter()
                .rev()
                .filter(|placed| self.stack.visible(placed))
                .find_map(|placed| {
                    self.tree_hit(
                        placed.client,
                        placed.surface,
                        (placed.x, placed.y),
                        point,
                        Some(placed.generation),
                    )
                })
        };
        let windows = self.stack.windows();
        let split = self.fullscreen_start().unwrap_or(windows.len());
        let (below, above) = windows.split_at(split);
        self.ime_popup_box(boxes)
            .and_then(|((client, surface), origin)| {
                self.tree_hit(client, surface, origin, point, None)
            })
            .or_else(|| layer_hit(Layer::Overlay..=Layer::Overlay))
            .or_else(|| self.popup_hit(point, above))
            .or_else(|| window_hit(above))
            .or_else(|| layer_hit(Layer::Top..=Layer::Top))
            .or_else(|| self.popup_hit(point, below))
            .or_else(|| window_hit(below))
            .or_else(|| layer_hit(Layer::Background..=Layer::Bottom))
    }
}
