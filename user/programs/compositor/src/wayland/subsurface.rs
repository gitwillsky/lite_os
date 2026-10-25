//! `wl_subcompositor`：把 surface 挂在父 surface 之下，随父窗口一起绘制。
//!
//! 位置请求在父 surface 下一次提交时生效；同步/异步（`set_sync`/`set_desync`）不单独缓存内容，
//! 子 surface 的提交立即可见——这是已记录的简化，Wine 的子窗口按这个模型工作。

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
    pub parent: ObjectId,
    /// 在父 surface 局部坐标里已生效的位置（逻辑像素）。
    pub position: (i32, i32),
    /// 等父 surface 下次提交生效的位置。
    pub pending: Option<(i32, i32)>,
}

/// 一个待绘制的子 surface：相对根 surface 左上角的逻辑位置、内容与 buffer scale。
pub struct ChildView<'a> {
    pub x: i32,
    pub y: i32,
    pub content: &'a Content,
    pub scale: i32,
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
                for _ in 0..=MAX_DEPTH {
                    let Some(current) = ancestor else { break };
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
                        .map(|s| s.parent);
                }
                self.surface_mut(parent)?;
                self.create(
                    id,
                    Interface::WlSubsurface,
                    version,
                    Data::Subsurface(surface),
                )?;
                self.surface_mut(surface)?.subsurface = Some(Subsurface {
                    parent,
                    position: (0, 0),
                    pending: None,
                });
                self.surface_mut(parent)?.children.push(surface);
                Ok(())
            }
        }
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
            // 不单独缓存同步状态，见模块说明。
            wl_subsurface::Request::SetSync | wl_subsurface::Request::SetDesync => Ok(()),
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
        let parent = self
            .surface_mut(surface)?
            .subsurface
            .as_ref()
            .map(|sub| sub.parent)
            .ok_or_else(|| Fail::implementation(object))?;
        let children = &mut self.surface_mut(parent)?.children;
        children.retain(|child| *child != surface);
        let position = if sibling == parent {
            // 相对父本身：父的内容总在子之下，"之上"等于最底，"之下"同样放最底（不画到父下面）。
            Some(0)
        } else {
            children
                .iter()
                .position(|child| *child == sibling)
                .map(|index| if above { index + 1 } else { index })
        };
        match position {
            Some(index) => {
                children.insert(index, surface);
                Ok(())
            }
            None => {
                children.push(surface);
                Err(Fail::new(
                    object,
                    wl_subsurface::Error::BadSurface as u32,
                    "sibling is not a child of the same parent",
                ))
            }
        }
    }

    /// 把 `surface` 从它的父的子列表里摘掉，并取消子 surface 的角色（内容随之不再绘制）。
    pub(super) fn detach_subsurface(&mut self, surface: ObjectId) {
        let parent = match self.surface_mut(surface) {
            Ok(state) => state.subsurface.take().map(|sub| sub.parent),
            Err(_) => None,
        };
        if let Some(parent) = parent
            && let Ok(parent) = self.surface_mut(parent)
        {
            parent.children.retain(|child| *child != surface);
        }
    }

    /// 父 surface 提交：待生效的子位置落地。
    pub(super) fn apply_child_positions(&mut self, parent: ObjectId) {
        let children = match self.surface_mut(parent) {
            Ok(state) => state.children.clone(),
            Err(_) => return,
        };
        for child in children {
            if let Ok(state) = self.surface_mut(child)
                && let Some(sub) = &mut state.subsurface
                && let Some(position) = sub.pending.take()
            {
                sub.position = position;
            }
        }
    }

    /// `root` 的全部可见后代，按绘制顺序（先父后子、兄弟自底向上），位置相对 `root` 左上角。
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
        if depth >= MAX_DEPTH {
            return;
        }
        let Some(Entry {
            data: Data::Surface(parent_state),
            ..
        }) = self.objects.get(&parent.get())
        else {
            return;
        };
        for child in &parent_state.children {
            let Some(Entry {
                data: Data::Surface(state),
                ..
            }) = self.objects.get(&child.get())
            else {
                continue;
            };
            let Some(sub) = &state.subsurface else {
                continue;
            };
            let position = (origin.0 + sub.position.0, origin.1 + sub.position.1);
            if let Some(content) = &state.content {
                views.push(ChildView {
                    x: position.0,
                    y: position.1,
                    content,
                    scale: state.scale,
                });
            }
            self.collect_children(*child, position, depth + 1, views);
        }
    }
}
