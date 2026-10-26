//! `wl_subcompositor`：把 surface 挂在父 surface 之下，随父窗口一起绘制。
//!
//! 同步模式与祖先继承决定缓存提交；父提交原子发布内容、位置和堆叠。

use wayland_wire::{
    protocol::{Interface, wl_subcompositor, wl_subsurface},
    wire::ObjectId,
};

use super::{
    Content,
    client::{Client, Data, Entry, Fail},
};

/// 子 surface 的角色状态，放在子 surface 里。
pub struct Subsurface {
    pub parent: Option<ObjectId>,
    /// 新子 surface 默认同步；祖先同步会覆盖本地 desync。
    pub synchronized: bool,
    /// 在父 surface 局部坐标里已生效的位置（逻辑像素）。
    pub position: (i32, i32),
    /// 等父 surface 下次提交生效的位置。
    pub pending: Option<(i32, i32)>,
}

/// 一个待绘制的子 surface：相对根 surface 原点的逻辑位置与已完成坐标投影的内容。
pub struct ChildView<'a> {
    pub surface: ObjectId,
    pub x: i32,
    pub y: i32,
    pub content: &'a Content,
}

/// 子树深度上限：恶意客户端不能用深层嵌套拖慢每帧遍历。
const MAX_DEPTH: usize = 8;

impl Client {
    pub(super) fn subcompositor(
        &mut self,
        object: ObjectId,
        version: u32,
        request: wl_subcompositor::Request,
    ) -> Result<(), Fail> {
        match request {
            wl_subcompositor::Request::Destroy => Ok(()),
            wl_subcompositor::Request::GetSubsurface {
                id,
                surface,
                parent,
            } => {
                let bad = |code: wl_subcompositor::Error, message: &str| {
                    Fail::new(object, code as u32, message.to_owned())
                };
                if surface == parent {
                    return Err(bad(
                        wl_subcompositor::Error::BadParent,
                        "a surface cannot be its own parent",
                    ));
                }
                let child = self.surface_mut(surface)?;
                if child.xdg.is_some()
                    || child.layer.is_some()
                    || child.cursor
                    || child.subsurface.is_some()
                {
                    return Err(bad(
                        wl_subcompositor::Error::BadSurface,
                        "wl_surface already has a role",
                    ));
                }
                // 父不能是自己的后代，否则遍历形成环。
                let mut ancestor = Some(parent);
                let mut remaining = MAX_DEPTH;
                for _ in 0..MAX_DEPTH {
                    let Some(current) = ancestor else { break };
                    remaining -= 1;
                    if current == surface {
                        return Err(bad(
                            wl_subcompositor::Error::BadParent,
                            "parent is a descendant of the surface",
                        ));
                    }
                    ancestor = self
                        .surface_mut(current)?
                        .subsurface
                        .as_ref()
                        .and_then(|s| s.parent);
                }
                if ancestor.is_some() || !self.subtree_fits(surface, remaining) {
                    return Err(Fail::new(
                        ObjectId::DISPLAY,
                        wayland_wire::protocol::wl_display::Error::NoMemory as u32,
                        "subsurface depth budget exhausted",
                    ));
                }
                self.surface_mut(parent)?;
                self.create(
                    id,
                    Interface::WlSubsurface,
                    version,
                    Data::Subsurface(surface),
                )?;
                self.surface_mut(surface)?.subsurface = Some(Subsurface {
                    parent: Some(parent),
                    synchronized: true,
                    position: (0, 0),
                    pending: None,
                });
                self.surface_mut(surface)?.subsurface_role = true;
                let parent_state = self.surface_mut(parent)?;
                parent_state.children.push(surface);
                let order = parent_state
                    .cached
                    .as_ref()
                    .map_or(&parent_state.current.order, |cached| &cached.state.order)
                    .clone();
                parent_state
                    .pending
                    .order
                    .get_or_insert(order)
                    .push(surface);
                Ok(())
            }
        }
    }

    // 挂接一棵已有子树时也要计入后代深度；只检查新父的祖先会悄悄隐藏超过绘制预算的后代。
    fn subtree_fits(&self, surface: ObjectId, remaining: usize) -> bool {
        let Some(Entry {
            data: Data::Surface(state),
            ..
        }) = self.objects.get(&surface.get())
        else {
            return false;
        };
        state
            .children
            .iter()
            .all(|child| remaining > 0 && self.subtree_fits(*child, remaining - 1))
    }

    pub(super) fn subsurface(
        &mut self,
        object: ObjectId,
        request: wl_subsurface::Request,
    ) -> Result<(), Fail> {
        let surface = match self.objects.get(&object.get()) {
            Some(Entry {
                data: Data::Subsurface(surface),
                ..
            }) => *surface,
            _ => return Err(Fail::implementation(object)),
        };
        match request {
            wl_subsurface::Request::Destroy => {
                self.detach_subsurface(surface);
                Ok(())
            }
            wl_subsurface::Request::SetPosition { x, y } => {
                if let Ok(state) = self.surface_mut(surface)
                    && let Some(sub) = &mut state.subsurface
                {
                    sub.pending = Some((x, y));
                }
                Ok(())
            }
            wl_subsurface::Request::PlaceAbove { sibling } => {
                self.reorder(object, surface, sibling, true)
            }
            wl_subsurface::Request::PlaceBelow { sibling } => {
                self.reorder(object, surface, sibling, false)
            }
            wl_subsurface::Request::SetSync => {
                if let Some(sub) = &mut self.surface_mut(surface)?.subsurface {
                    sub.synchronized = true;
                }
                Ok(())
            }
            wl_subsurface::Request::SetDesync => {
                if let Some(sub) = &mut self.surface_mut(surface)?.subsurface {
                    sub.synchronized = false;
                }
                self.flush_desynchronized(surface)
            }
        }
    }

    /// 在兄弟间重排：`sibling` 必须是同一父的另一个子 surface，或者父本身（此时放到最前/最后）。
    fn reorder(
        &mut self,
        object: ObjectId,
        surface: ObjectId,
        sibling: ObjectId,
        above: bool,
    ) -> Result<(), Fail> {
        let Some(parent) = self
            .surface_mut(surface)?
            .subsurface
            .as_ref()
            .and_then(|sub| sub.parent)
        else {
            return Ok(());
        };
        let state = self.surface_mut(parent)?;
        if sibling == surface || (sibling != parent && !state.children.contains(&sibling)) {
            return Err(Fail::new(
                object,
                wl_subsurface::Error::BadSurface as u32,
                "sibling is not another child of the same parent",
            ));
        }
        let order = state
            .cached
            .as_ref()
            .map_or(&state.current.order, |cached| &cached.state.order)
            .clone();
        let children = state.pending.order.get_or_insert(order);
        children.retain(|child| *child != surface);
        let position = children
            .iter()
            .position(|child| *child == sibling)
            .ok_or_else(|| {
                Fail::new(
                    object,
                    wl_subsurface::Error::BadSurface as u32,
                    "sibling has not been added to parent state",
                )
            })?;
        children.insert(position + usize::from(above), surface);
        Ok(())
    }

    /// 把 `surface` 从它的父的子列表里摘掉，并取消子 surface 的角色（内容随之不再绘制）。
    pub(super) fn detach_subsurface(&mut self, surface: ObjectId) {
        let parent = match self.surface_mut(surface) {
            Ok(state) => {
                state.current.content = None;
                if let Some(cached) = state.cached.take() {
                    state.queued_callbacks.extend(cached.callbacks);
                }
                state.subsurface.take().and_then(|sub| sub.parent)
            }
            Err(_) => None,
        };
        if parent.is_some() {
            self.commits += 1;
        }
        if let Some(parent) = parent
            && let Ok(parent) = self.surface_mut(parent)
        {
            parent.children.retain(|child| *child != surface);
            parent.current.order.retain(|child| *child != surface);
            if let Some(order) = &mut parent.pending.order {
                order.retain(|child| *child != surface);
            }
            if let Some(cached) = &mut parent.cached {
                cached.state.order.retain(|child| *child != surface);
            }
        }
    }

    /// 父被销毁时保留仍存活的 wl_subsurface 对象，但断开父引用并取消映射。
    pub(super) fn orphan_children(&mut self, parent: ObjectId) {
        let children = self
            .surface_mut(parent)
            .map(|state| state.children.clone())
            .unwrap_or_default();
        for child in children {
            if let Ok(state) = self.surface_mut(child) {
                if let Some(sub) = &mut state.subsurface {
                    sub.parent = None;
                }
                state.current.content = None;
                if let Some(cached) = state.cached.take() {
                    state.queued_callbacks.extend(cached.callbacks);
                }
            }
        }
    }

    /// 从父提交冻结位置；请求随后再次修改同一子位置时不会污染已缓存的父提交。
    pub(super) fn take_child_positions(
        &mut self,
        parent: ObjectId,
    ) -> std::collections::BTreeMap<ObjectId, (i32, i32)> {
        let children = self
            .surface_mut(parent)
            .map(|state| state.children.clone())
            .unwrap_or_default();
        children
            .into_iter()
            .filter_map(|child| {
                Some((
                    child,
                    self.surface_mut(child)
                        .ok()?
                        .subsurface
                        .as_mut()?
                        .pending
                        .take()?,
                ))
            })
            .collect()
    }

    /// 子 surface 的键盘/窗口管理目标是最顶层祖先，自身只能获得指针焦点。
    pub(super) fn root_surface(&self, surface: ObjectId) -> Option<ObjectId> {
        let mut root = surface;
        for _ in 0..=MAX_DEPTH {
            let Entry {
                data: Data::Surface(state),
                ..
            } = self.objects.get(&root.get())?
            else {
                return None;
            };
            match &state.subsurface {
                None => return Some(root),
                Some(sub) => root = sub.parent?,
            }
        }
        None
    }

    pub(super) fn effectively_synchronized(&self, surface: ObjectId) -> bool {
        let mut current = surface;
        for _ in 0..=MAX_DEPTH {
            let Some(Entry {
                data: Data::Surface(state),
                ..
            }) = self.objects.get(&current.get())
            else {
                return false;
            };
            let Some(sub) = &state.subsurface else {
                return false;
            };
            if sub.synchronized {
                return true;
            }
            let Some(parent) = sub.parent else {
                return false;
            };
            current = parent;
        }
        false
    }

    /// 父的当前状态生效后递归发布同步孩子；没有缓存也必须向下传递祖先的提交。
    pub(super) fn apply_synchronized_children(&mut self, parent: ObjectId) -> Result<(), Fail> {
        let children = self.surface_mut(parent)?.children.clone();
        for child in children {
            if !self.effectively_synchronized(child) {
                continue;
            }
            if let Some(commit) = self.surface_mut(child)?.cached.take() {
                self.apply_surface_commit(child, commit)?;
            }
            self.apply_synchronized_children(child)?;
        }
        Ok(())
    }

    fn flush_desynchronized(&mut self, surface: ObjectId) -> Result<(), Fail> {
        if self.effectively_synchronized(surface) {
            return Ok(());
        }
        if let Some(commit) = self.surface_mut(surface)?.cached.take() {
            self.apply_surface_commit(surface, commit)?;
            self.apply_synchronized_children(surface)?;
        }
        let children = self.surface_mut(surface)?.children.clone();
        for child in children {
            self.flush_desynchronized(child)?;
        }
        Ok(())
    }

    /// 按已提交堆叠顺序投影整棵树，包含根本身；未映射的祖先使全部后代不可见。
    pub fn subtree(&self, root: ObjectId) -> Vec<ChildView<'_>> {
        let mut views = Vec::new();
        self.collect_children(root, (0, 0), 0, &mut views);
        views
    }

    fn collect_children<'a>(
        &'a self,
        parent: ObjectId,
        origin: (i32, i32),
        depth: usize,
        views: &mut Vec<ChildView<'a>>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let Some(Entry {
            data: Data::Surface(state),
            ..
        }) = self.objects.get(&parent.get())
        else {
            return;
        };
        let Some(content) = &state.current.content else {
            return;
        };
        for surface in &state.current.order {
            if *surface == parent {
                views.push(ChildView {
                    surface: parent,
                    x: origin.0,
                    y: origin.1,
                    content,
                });
            } else if let Some(Entry {
                data: Data::Surface(child),
                ..
            }) = self.objects.get(&surface.get())
                && let Some(sub) = child
                    .subsurface
                    .as_ref()
                    .filter(|sub| sub.parent == Some(parent))
            {
                self.collect_children(
                    *surface,
                    (
                        origin.0.saturating_add(sub.position.0),
                        origin.1.saturating_add(sub.position.1),
                    ),
                    depth + 1,
                    views,
                );
            }
        }
    }
}
