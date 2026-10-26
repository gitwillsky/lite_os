//! `zwlr_layer_shell_v1`：桌面外壳的 surface（桌面背景、任务栏、登录屏）。
//!
//! 不是应用窗口：位置由锚点、边距与输出大小决定，层级固定在应用窗口之下或之上。

use wayland_wire::{
    protocol::{
        Interface, wl_display, zwlr_layer_shell_v1 as shell, zwlr_layer_surface_v1 as layer_surface,
    },
    wire::ObjectId,
};

use super::{
    Content,
    client::{Client, Data, Entry, Fail},
};

/// 锚点位（`zwlr_layer_surface_v1.anchor`）。
pub const ANCHOR_TOP: u32 = 1;
pub const ANCHOR_BOTTOM: u32 = 2;
pub const ANCHOR_LEFT: u32 = 4;
pub const ANCHOR_RIGHT: u32 = 8;
const ANCHOR_ALL: u32 = ANCHOR_TOP | ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT;

/// 层级，自底向上。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// 键盘交互方式：不参与、独占与点击获得焦点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardInteractivity {
    None,
    /// 独占键盘焦点，用于登录屏。
    Exclusive,
    OnDemand,
}

/// 一个 layer surface 的可协商状态；`commit` 之前写入 `pending`，之后成为 `current`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerState {
    pub layer: Layer,
    pub anchor: u32,
    /// 客户端请求的逻辑尺寸；0 表示"填满锚定的两条对边之间"。
    pub width: u32,
    pub height: u32,
    /// `top, right, bottom, left`，逻辑像素。
    pub margin: [i32; 4],
    pub exclusive_zone: i32,
    /// 独占区所在的边（锚点位）；0 表示由锚点推断。
    pub exclusive_edge: u32,
    pub keyboard: KeyboardInteractivity,
}

pub struct LayerSurface {
    pub surface: ObjectId,
    pub namespace: String,
    pending: LayerState,
    pub current: LayerState,
    outstanding: Vec<u32>,
    /// 本轮映射已确认 configure；缺少它会允许首次协商前提交 buffer。
    acked: bool,
    /// 本轮已发初始 configure；缺少它会在每次 commit 重复协商。
    initial_sent: bool,
}

impl LayerSurface {
    fn reset(&mut self) {
        self.pending = LayerState {
            layer: self.current.layer,
            anchor: 0,
            width: 0,
            height: 0,
            margin: [0; 4],
            exclusive_zone: 0,
            exclusive_edge: 0,
            keyboard: KeyboardInteractivity::None,
        };
        self.current = self.pending;
        self.outstanding.clear();
        self.acked = false;
        self.initial_sent = false;
    }
}

/// 一个已映射的 layer surface。
pub struct LayerView<'a> {
    pub surface: ObjectId,
    pub namespace: &'a str,
    pub state: &'a LayerState,
    pub content: &'a Content,
}

/// 按锚点与边距算出要告诉客户端的逻辑尺寸；`output` 是输出的逻辑尺寸。
///
/// 请求的尺寸非零就用它；为零则填满被锚定的两条对边之间（扣除两侧边距）。
pub fn configure_size(state: &LayerState, output: (i32, i32)) -> (u32, u32) {
    let [top, right, bottom, left] = state.margin;
    let fill = |requested: u32, extent: i32, near: i32, far: i32| {
        if requested != 0 {
            requested
        } else {
            (i64::from(extent) - i64::from(near) - i64::from(far)).clamp(0, i64::from(u32::MAX))
                as u32
        }
    };
    (
        fill(state.width, output.0, left, right),
        fill(state.height, output.1, top, bottom),
    )
}

/// 独占区作用的边：`exclusive_edge` 优先；否则锚点恰好是单边，或三边（缺失的对边之外的那条中间边）。
pub fn exclusive_side(state: &LayerState) -> Option<u32> {
    if state.exclusive_edge != 0 {
        return Some(state.exclusive_edge);
    }
    match state.anchor {
        single @ (ANCHOR_TOP | ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT) => Some(single),
        three if three.count_ones() == 3 => [ANCHOR_TOP, ANCHOR_BOTTOM, ANCHOR_LEFT, ANCHOR_RIGHT]
            .into_iter()
            .find(|edge| three & edge == 0)
            .map(|missing| match missing {
                ANCHOR_TOP => ANCHOR_BOTTOM,
                ANCHOR_BOTTOM => ANCHOR_TOP,
                ANCHOR_LEFT => ANCHOR_RIGHT,
                _ => ANCHOR_LEFT,
            }),
        _ => None,
    }
}

fn layer_of(value: shell::Layer) -> Layer {
    match value {
        shell::Layer::Background => Layer::Background,
        shell::Layer::Bottom => Layer::Bottom,
        shell::Layer::Top => Layer::Top,
        shell::Layer::Overlay => Layer::Overlay,
    }
}

fn interactivity_of(value: layer_surface::KeyboardInteractivity) -> KeyboardInteractivity {
    match value {
        layer_surface::KeyboardInteractivity::None => KeyboardInteractivity::None,
        layer_surface::KeyboardInteractivity::Exclusive => KeyboardInteractivity::Exclusive,
        layer_surface::KeyboardInteractivity::OnDemand => KeyboardInteractivity::OnDemand,
    }
}

impl Client {
    pub(super) fn layer_shell(
        &mut self,
        object: ObjectId,
        version: u32,
        request: shell::Request,
    ) -> Result<(), Fail> {
        let shell::Request::GetLayerSurface {
            id,
            surface,
            layer,
            namespace,
            ..
        } = request
        else {
            return Ok(());
        };
        let state = self.surface_mut(surface)?;
        if state.xdg.is_some() || state.layer.is_some() || state.cursor {
            return Err(Fail::new(
                object,
                shell::Error::Role as u32,
                "wl_surface already has a role",
            ));
        }
        if state.current.content.is_some() || state.pending.buffer.is_some() {
            return Err(Fail::new(
                object,
                shell::Error::AlreadyConstructed as u32,
                "wl_surface already has a buffer attached",
            ));
        }
        let initial = LayerState {
            layer: layer_of(layer),
            anchor: 0,
            width: 0,
            height: 0,
            margin: [0; 4],
            exclusive_zone: 0,
            exclusive_edge: 0,
            keyboard: KeyboardInteractivity::None,
        };
        let layer_surface = LayerSurface {
            surface,
            namespace,
            pending: initial,
            current: initial,
            outstanding: Vec::new(),
            acked: false,
            initial_sent: false,
        };
        self.create(
            id,
            Interface::ZwlrLayerSurfaceV1,
            version,
            Data::LayerSurface(layer_surface),
        )?;
        self.surface_mut(surface)?.layer = Some(id);
        Ok(())
    }

    fn layer_surface_mut(&mut self, object: ObjectId) -> Result<&mut LayerSurface, Fail> {
        match self.objects.get_mut(&object.get()) {
            Some(Entry {
                data: Data::LayerSurface(layer),
                ..
            }) => Ok(layer),
            _ => Err(Fail::implementation(object)),
        }
    }

    pub(super) fn layer_surface(
        &mut self,
        object: ObjectId,
        request: layer_surface::Request,
    ) -> Result<(), Fail> {
        use layer_surface::Request as R;
        let invalid = |code: layer_surface::Error, message: &str| {
            Fail::new(object, code as u32, message.to_owned())
        };
        match request {
            R::SetSize { width, height } => {
                let layer = self.layer_surface_mut(object)?;
                (layer.pending.width, layer.pending.height) = (width, height);
            }
            R::SetAnchor { anchor } => {
                if anchor & !ANCHOR_ALL != 0 {
                    return Err(invalid(
                        layer_surface::Error::InvalidAnchor,
                        "unknown anchor bits",
                    ));
                }
                self.layer_surface_mut(object)?.pending.anchor = anchor;
            }
            R::SetExclusiveZone { zone } => {
                self.layer_surface_mut(object)?.pending.exclusive_zone = zone
            }
            R::SetMargin {
                top,
                right,
                bottom,
                left,
            } => self.layer_surface_mut(object)?.pending.margin = [top, right, bottom, left],
            R::SetKeyboardInteractivity {
                keyboard_interactivity,
            } => {
                self.layer_surface_mut(object)?.pending.keyboard =
                    interactivity_of(keyboard_interactivity);
            }
            R::GetPopup { .. } => {
                return Err(Fail::new(
                    ObjectId::DISPLAY,
                    wl_display::Error::Implementation as u32,
                    "layer-shell popups are not supported yet",
                ));
            }
            R::AckConfigure { serial } => {
                let layer = self.layer_surface_mut(object)?;
                let Some(position) = layer.outstanding.iter().position(|sent| *sent == serial)
                else {
                    return Err(invalid(
                        layer_surface::Error::InvalidSurfaceState,
                        "configure serial was not sent or already acknowledged",
                    ));
                };
                layer.outstanding.drain(..=position);
                layer.acked = true;
            }
            R::Destroy => {
                let surface = self.layer_surface_mut(object)?.surface;
                // 窗口随角色一起消失；wl_surface 本身可能已先被销毁。
                if let Ok(surface) = self.surface_mut(surface) {
                    surface.layer = None;
                    surface.current.content = None;
                }
            }
            R::SetLayer { layer } => {
                self.layer_surface_mut(object)?.pending.layer = layer_of(layer)
            }
            R::SetExclusiveEdge { edge } => {
                if edge != 0 && (edge.count_ones() != 1 || edge & !ANCHOR_ALL != 0) {
                    return Err(invalid(
                        layer_surface::Error::InvalidExclusiveEdge,
                        "exclusive edge must be a single edge",
                    ));
                }
                self.layer_surface_mut(object)?.pending.exclusive_edge = edge;
            }
        }
        Ok(())
    }

    /// commit 之前只校验 pending；角色状态随 surface 发布，缓冲必须在首次 configure 被确认之后才能附着。
    pub(super) fn layer_check_commit(&mut self, surface: ObjectId) -> Result<(), Fail> {
        let state = self.surface_mut(surface)?;
        let (Some(layer_id), has_buffer) =
            (state.layer, matches!(state.pending.buffer, Some(Some(_))))
        else {
            return Ok(());
        };
        let layer = self.layer_surface_mut(layer_id)?;
        let pending = layer.pending;
        if pending.exclusive_edge & pending.anchor != pending.exclusive_edge {
            return Err(Fail::new(
                layer_id,
                layer_surface::Error::InvalidExclusiveEdge as u32,
                "exclusive edge must be anchored",
            ));
        }
        let fills = |requested: u32, low: u32, high: u32| {
            requested != 0 || pending.anchor & low != 0 && pending.anchor & high != 0
        };
        if !fills(pending.width, ANCHOR_LEFT, ANCHOR_RIGHT)
            || !fills(pending.height, ANCHOR_TOP, ANCHOR_BOTTOM)
        {
            return Err(Fail::new(
                layer_id,
                layer_surface::Error::InvalidSize as u32,
                "a zero size needs both opposite edges anchored",
            ));
        }
        if has_buffer && !layer.acked {
            return Err(Fail::new(
                layer_id,
                layer_surface::Error::InvalidSurfaceState as u32,
                "buffer attached before the first configure was acknowledged",
            ));
        }
        Ok(())
    }

    /// commit 之后：第一次提交，或尺寸相关状态改变时，发送 configure。
    pub(super) fn layer_after_commit(
        &mut self,
        surface: ObjectId,
        previous: Option<LayerState>,
    ) -> Result<(), Fail> {
        let Some(layer_id) = self.surface_mut(surface)?.layer else {
            return Ok(());
        };
        let output = (
            self.output.width / self.output.scale.max(1),
            self.output.height / self.output.scale.max(1),
        );
        let layer = self.layer_surface_mut(layer_id)?;
        // 唯一发布点在此；提前覆盖 current 会丢失旧尺寸，导致后续 configure 判断永远不变。
        layer.current = layer.pending;
        let changed = previous.is_some_and(|previous| {
            configure_size(&previous, output) != configure_size(&layer.current, output)
        });
        if layer.initial_sent && !changed {
            return Ok(());
        }
        layer.initial_sent = true;
        let (width, height) = configure_size(&layer.current, output);
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        self.layer_surface_mut(layer_id)?.outstanding.push(serial);
        layer_surface::Event::Configure {
            serial,
            width,
            height,
        }
        .encode(layer_id, &mut self.writer)?;
        Ok(())
    }

    /// 取出 surface 当前生效的 layer 状态（若有）；供 commit 前后比较。
    pub(super) fn layer_state(&mut self, surface: ObjectId) -> Option<LayerState> {
        let layer_id = self.surface_mut(surface).ok()?.layer?;
        Some(self.layer_surface_mut(layer_id).ok()?.current)
    }

    /// 空 buffer 撤销映射后恢复初始状态；下一次无 buffer commit 才启动新的 configure 握手。
    pub(super) fn layer_unmap(&mut self, surface: ObjectId) {
        if let Some(id) = self.surface_mut(surface).ok().and_then(|state| state.layer)
            && let Ok(layer) = self.layer_surface_mut(id)
        {
            layer.reset();
        }
    }

    /// 全部已映射的 layer surface。
    pub fn layers(&self) -> impl Iterator<Item = LayerView<'_>> {
        self.objects.values().filter_map(move |entry| {
            let Data::LayerSurface(layer) = &entry.data else {
                return None;
            };
            let Some(Entry {
                data: Data::Surface(surface),
                ..
            }) = self.objects.get(&layer.surface.get())
            else {
                return None;
            };
            Some(LayerView {
                surface: layer.surface,
                namespace: &layer.namespace,
                state: &layer.current,
                content: surface.current.content.as_ref()?,
            })
        })
    }
}
