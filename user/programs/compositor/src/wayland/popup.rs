//! `xdg_popup` 与 `xdg_positioner`：菜单、下拉框这类依附于父窗口的弹出 surface。
//!
//! 位置由 positioner 的锚点矩形、锚点、重力与偏移算出，相对父 surface 的左上角（不使用窗口几何）；
//! 不做越界调整（`constraint_adjustment` 只记录不执行）。`grab` 记下弹出层持有者：
//! 之后点击弹出层之外时合成器发 `popup_done`，由客户端销毁它。

use wayland_wire::{
    protocol::{
        Interface, xdg_popup, xdg_positioner,
        xdg_positioner::{Anchor, Gravity},
        xdg_surface, xdg_wm_base,
    },
    wire::ObjectId,
};

use super::client::{Client, Data, Entry, Fail};

/// positioner 累积的参数。
#[derive(Clone, Copy, Default)]
pub struct Positioner {
    size: (i32, i32),
    anchor_rect: (i32, i32, i32, i32),
    anchor: u32,
    gravity: u32,
    offset: (i32, i32),
}

impl Positioner {
    /// 弹出层相对父 surface 左上角的位置与尺寸。
    ///
    /// 锚点取自锚点矩形的边或角；重力决定弹出层相对锚点向哪边伸展，默认向右下。
    fn place(&self) -> (i32, i32, i32, i32) {
        let (rx, ry, rw, rh) = self.anchor_rect;
        let anchor_x = match self.anchor {
            a if a == Anchor::Left as u32
                || a == Anchor::TopLeft as u32
                || a == Anchor::BottomLeft as u32 =>
            {
                rx
            }
            a if a == Anchor::Right as u32
                || a == Anchor::TopRight as u32
                || a == Anchor::BottomRight as u32 =>
            {
                rx + rw
            }
            _ => rx + rw / 2,
        };
        let anchor_y = match self.anchor {
            a if a == Anchor::Top as u32
                || a == Anchor::TopLeft as u32
                || a == Anchor::TopRight as u32 =>
            {
                ry
            }
            a if a == Anchor::Bottom as u32
                || a == Anchor::BottomLeft as u32
                || a == Anchor::BottomRight as u32 =>
            {
                ry + rh
            }
            _ => ry + rh / 2,
        };
        let (width, height) = self.size;
        let x = match self.gravity {
            g if g == Gravity::Left as u32
                || g == Gravity::TopLeft as u32
                || g == Gravity::BottomLeft as u32 =>
            {
                anchor_x - width
            }
            g if g == Gravity::Right as u32
                || g == Gravity::TopRight as u32
                || g == Gravity::BottomRight as u32 =>
            {
                anchor_x
            }
            _ => anchor_x - width / 2,
        };
        let y = match self.gravity {
            g if g == Gravity::Top as u32
                || g == Gravity::TopLeft as u32
                || g == Gravity::TopRight as u32 =>
            {
                anchor_y - height
            }
            g if g == Gravity::Bottom as u32
                || g == Gravity::BottomLeft as u32
                || g == Gravity::BottomRight as u32 =>
            {
                anchor_y
            }
            _ => anchor_y - height / 2,
        };
        (x + self.offset.0, y + self.offset.1, width, height)
    }
}

/// 一个弹出层的角色状态，放在 `xdg_popup` 对象里。
pub struct Popup {
    pub xdg_surface: ObjectId,
    /// 父 `xdg_surface` 的 `wl_surface`。
    pub parent: ObjectId,
    /// 相对父 surface 的位置（逻辑像素）。
    pub position: (i32, i32),
    /// positioner 给出的尺寸，初始 configure 带上它。
    pub size: (i32, i32),
    /// 是否 `grab` 过：点击外部时要收到 `popup_done`。
    pub grabbed: bool,
}

/// 一个待绘制与命中的弹出层：位置相对**根窗口**的 surface（已沿父链累加）。
pub struct PopupView {
    pub client_root: ObjectId,
    pub surface: ObjectId,
    pub x: i32,
    pub y: i32,
}

/// 父链深度上限：弹出层嵌套不会这么深，防止客户端造环让遍历不终止。
const MAX_DEPTH: usize = 8;

impl Client {
    pub(super) fn positioner(
        &mut self,
        object: ObjectId,
        request: xdg_positioner::Request,
    ) -> Result<(), Fail> {
        let invalid = |message: &str| {
            Fail::new(
                object,
                xdg_positioner::Error::InvalidInput as u32,
                message.to_owned(),
            )
        };
        let Some(Entry {
            data: Data::Positioner(state),
            ..
        }) = self.objects.get_mut(&object.get())
        else {
            return Err(Fail::implementation(object));
        };
        use xdg_positioner::Request as R;
        match request {
            R::Destroy => {}
            R::SetSize { width, height } => {
                if width <= 0 || height <= 0 {
                    return Err(invalid("size must be positive"));
                }
                state.size = (width, height);
            }
            R::SetAnchorRect {
                x,
                y,
                width,
                height,
            } => {
                if width < 0 || height < 0 {
                    return Err(invalid("anchor rectangle must not be negative"));
                }
                state.anchor_rect = (x, y, width, height);
            }
            R::SetAnchor { anchor } => state.anchor = anchor as u32,
            R::SetGravity { gravity } => state.gravity = gravity as u32,
            R::SetOffset { x, y } => state.offset = (x, y),
            // 只记录不执行：不做越界调整，也不随父窗口重新定位。
            R::SetConstraintAdjustment { .. }
            | R::SetReactive
            | R::SetParentSize { .. }
            | R::SetParentConfigure { .. } => {}
        }
        Ok(())
    }

    fn positioner_state(&self, positioner: ObjectId) -> Result<Positioner, Fail> {
        match self.objects.get(&positioner.get()) {
            Some(Entry {
                data: Data::Positioner(state),
                ..
            }) => Ok(*state),
            _ => Err(Fail::invalid_object(format!(
                "object {} is not an xdg_positioner",
                positioner.get()
            ))),
        }
    }

    pub(super) fn create_positioner(&mut self, id: ObjectId, version: u32) -> Result<(), Fail> {
        self.create(
            id,
            Interface::XdgPositioner,
            version,
            Data::Positioner(Positioner::default()),
        )
    }

    /// `xdg_surface.get_popup`：为 `xdg` 建立弹出层角色。
    pub(super) fn get_popup(
        &mut self,
        xdg: ObjectId,
        version: u32,
        id: ObjectId,
        parent: Option<ObjectId>,
        positioner: ObjectId,
    ) -> Result<(), Fail> {
        let Some(parent) = parent else {
            return Err(Fail::new(
                xdg,
                xdg_wm_base::Error::InvalidPopupParent as u32,
                "popup needs an xdg_surface parent",
            ));
        };
        let parent_surface = match self.objects.get(&parent.get()) {
            Some(Entry {
                data: Data::XdgSurface(parent),
                ..
            }) => parent.surface,
            _ => {
                return Err(Fail::invalid_object("popup parent is not an xdg_surface"));
            }
        };
        let placed = self.positioner_state(positioner)?;
        if placed.size.0 <= 0 || placed.size.1 <= 0 {
            return Err(Fail::new(
                xdg,
                xdg_wm_base::Error::InvalidPositioner as u32,
                "positioner has no size",
            ));
        }
        let (x, y, width, height) = placed.place();
        let xdg_state = self.xdg_surface_mut(xdg)?;
        if xdg_state.toplevel.is_some() || xdg_state.popup.is_some() {
            return Err(Fail::new(
                xdg,
                xdg_surface::Error::AlreadyConstructed as u32,
                "xdg_surface already has a role",
            ));
        }
        xdg_state.popup = Some(id);
        self.create(
            id,
            Interface::XdgPopup,
            version,
            Data::Popup(Popup {
                xdg_surface: xdg,
                parent: parent_surface,
                position: (x, y),
                size: (width, height),
                grabbed: false,
            }),
        )
    }

    pub(super) fn xdg_popup(
        &mut self,
        object: ObjectId,
        request: xdg_popup::Request,
    ) -> Result<(), Fail> {
        let xdg = match self.objects.get(&object.get()) {
            Some(Entry {
                data: Data::Popup(popup),
                ..
            }) => popup.xdg_surface,
            _ => return Err(Fail::implementation(object)),
        };
        match request {
            xdg_popup::Request::Destroy => {
                if let Ok(state) = self.xdg_surface_mut(xdg) {
                    state.popup = None;
                    state.initial_sent = false;
                    state.acked = false;
                    state.outstanding.clear();
                    let surface = state.surface;
                    if let Ok(surface) = self.surface_mut(surface) {
                        surface.current.content = None;
                    }
                }
                Ok(())
            }
            xdg_popup::Request::Grab { .. } => {
                if let Some(Entry {
                    data: Data::Popup(popup),
                    ..
                }) = self.objects.get_mut(&object.get())
                {
                    popup.grabbed = true;
                }
                Ok(())
            }
            xdg_popup::Request::Reposition { positioner, token } => {
                let placed = self.positioner_state(positioner)?;
                let (x, y, width, height) = placed.place();
                if let Some(Entry {
                    data: Data::Popup(popup),
                    ..
                }) = self.objects.get_mut(&object.get())
                {
                    popup.position = (x, y);
                    popup.size = (width, height);
                }
                xdg_popup::Event::Repositioned { token }.encode(object, &mut self.writer)?;
                self.send_popup_configure(xdg, object, x, y, width, height)
            }
        }
    }

    /// 发送 `xdg_popup.configure` 与配套的 `xdg_surface.configure`。
    fn send_popup_configure(
        &mut self,
        xdg: ObjectId,
        popup: ObjectId,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), Fail> {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        self.xdg_surface_mut(xdg)?.outstanding.push(serial);
        xdg_popup::Event::Configure {
            x,
            y,
            width,
            height,
        }
        .encode(popup, &mut self.writer)?;
        xdg_surface::Event::Configure { serial }.encode(xdg, &mut self.writer)?;
        Ok(())
    }

    /// 弹出层 surface 第一次提交时发初始 configure（位置与尺寸来自 positioner）。
    pub(super) fn popup_after_commit(&mut self, xdg: ObjectId) -> Result<(), Fail> {
        let state = self.xdg_surface_mut(xdg)?;
        let Some(popup) = state.popup.filter(|_| !state.initial_sent) else {
            return Ok(());
        };
        state.initial_sent = true;
        let (position, size) = match self.objects.get(&popup.get()) {
            Some(Entry {
                data: Data::Popup(popup),
                ..
            }) => (popup.position, popup.size),
            _ => return Ok(()),
        };
        self.send_popup_configure(xdg, popup, position.0, position.1, size.0, size.1)
    }

    /// 本连接所有已映射弹出层，位置沿父链累加到根窗口 surface。
    pub fn popups(&self) -> Vec<PopupView> {
        self.objects
            .values()
            .filter_map(|entry| {
                let Data::Popup(popup) = &entry.data else {
                    return None;
                };
                let Some(Entry {
                    data: Data::XdgSurface(xdg),
                    ..
                }) = self.objects.get(&popup.xdg_surface.get())
                else {
                    return None;
                };
                let Some(Entry {
                    data: Data::Surface(surface),
                    ..
                }) = self.objects.get(&xdg.surface.get())
                else {
                    return None;
                };
                surface.current.content.as_ref()?;
                let (root, offset) = self.popup_root(popup)?;
                Some(PopupView {
                    client_root: root,
                    surface: xdg.surface,
                    x: offset.0 + popup.position.0,
                    y: offset.1 + popup.position.1,
                })
            })
            .collect()
    }

    /// 沿父链找到顶层窗口 surface，并累加途中弹出层的位置（不含 `popup` 自己）。
    fn popup_root(&self, popup: &Popup) -> Option<(ObjectId, (i32, i32))> {
        let mut parent = popup.parent;
        let mut offset = (0, 0);
        for _ in 0..MAX_DEPTH {
            let owner = self.objects.values().find_map(|entry| match &entry.data {
                Data::Popup(candidate)
                    if matches!(
                        self.objects.get(&candidate.xdg_surface.get()),
                        Some(Entry { data: Data::XdgSurface(x), .. }) if x.surface == parent
                    ) =>
                {
                    Some(candidate)
                }
                _ => None,
            });
            match owner {
                Some(owner) => {
                    offset.0 += owner.position.0;
                    offset.1 += owner.position.1;
                    parent = owner.parent;
                }
                None => return Some((parent, offset)),
            }
        }
        None
    }

    /// 点击了弹出层以外的位置：给所有 `grab` 过的弹出层发 `popup_done`（客户端随后销毁它们）。
    pub fn dismiss_grabbed_popups(&mut self) {
        let grabbed: Vec<ObjectId> = self
            .objects
            .iter()
            .filter_map(|(id, entry)| match &entry.data {
                Data::Popup(popup) if popup.grabbed => ObjectId::new(*id),
                _ => None,
            })
            .collect();
        for popup in grabbed {
            let _ = xdg_popup::Event::PopupDone.encode(popup, &mut self.writer);
        }
    }
}
