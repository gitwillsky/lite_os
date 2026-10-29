//! 按已提交 surface 树的绘制逆序做输入命中；窗口激活与指针焦点使用各自正确的目标。
use super::*;
use crate::desktop::Placement;

impl Session {
    /// 返回子 surface 对应的窗口/外壳根；popup 的键盘与窗口管理目标仍为其 xdg_toplevel。
    pub(super) fn root_target(&self, target: (ClientId, ObjectId)) -> Option<(ClientId, ObjectId)> {
        let root = self.server.root_surface(target.0, target.1)?;
        let root = self
            .server
            .popups()
            .into_iter()
            .find(|(client, popup)| *client == target.0 && popup.surface == root)
            .map_or(root, |(_, popup)| popup.client_root);
        Some((target.0, root))
    }

    /// 隐式抓取跟随真实 surface：窗口/外壳/popup 移动后仍向原来的子 surface 发送坐标。
    pub(super) fn locate_target(
        &self,
        boxes: &[LayerBox],
        target: (ClientId, ObjectId),
    ) -> Option<Hit> {
        let root = self.server.root_surface(target.0, target.1)?;
        let scale = self.scale as i32;
        let origin = boxes
            .iter()
            .find(|layer| (layer.client, layer.surface) == (target.0, root))
            .map(|layer| (layer.x, layer.y))
            .or_else(|| {
                self.stack
                    .placement(target.0, root)
                    .filter(|placed| !placed.minimized)
                    .map(|placed| (placed.x, placed.y))
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
                            .filter(|placed| !placed.minimized)?;
                        Some((
                            placed.x.saturating_add(popup.x.saturating_mul(scale)),
                            placed.y.saturating_add(popup.y.saturating_mul(scale)),
                        ))
                    })
            })?;
        self.server
            .subtree(target.0, root)
            .into_iter()
            .find(|leaf| leaf.surface == target.1)
            .map(|leaf| Hit {
                target,
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
                    root.client == client && root.surface == popup.client_root && !root.minimized
                })?;
                self.tree_hit(
                    client,
                    popup.surface,
                    (
                        root.x.saturating_add(popup.x.saturating_mul(scale)),
                        root.y.saturating_add(popup.y.saturating_mul(scale)),
                    ),
                    point,
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
                    self.tree_hit(layer.client, layer.surface, (layer.x, layer.y), point)
                })
        };
        let window_hit = |windows: &[Placement]| {
            windows
                .iter()
                .rev()
                .filter(|placed| !placed.minimized)
                .find_map(|placed| {
                    self.tree_hit(placed.client, placed.surface, (placed.x, placed.y), point)
                })
        };
        let windows = self.stack.windows();
        let split = self.stack.fullscreen_start().unwrap_or(windows.len());
        let (below, above) = windows.split_at(split);
        layer_hit(Layer::Overlay..=Layer::Overlay)
            .or_else(|| self.popup_hit(point, above))
            .or_else(|| window_hit(above))
            .or_else(|| layer_hit(Layer::Top..=Layer::Top))
            .or_else(|| self.popup_hit(point, below))
            .or_else(|| window_hit(below))
            .or_else(|| layer_hit(Layer::Background..=Layer::Bottom))
    }
}
