//! 客户端装饰的命中与输入策略；几何和按钮位置与 Painter 共用同一份事实。

use std::time::{Duration, Instant};

use crate::Bitmap;
use crate::wayland::{CursorShape, ResizeEdge};
use crate::{
    CaptionButton, Configuration, EventKind, FrameKind, Painter, WindowRequest,
    window::{CAPTION_HEIGHT, caption_button_rects},
};
use canvas::Rect;

/// 装饰产生的操作；关闭交给应用处理，其他操作经 Display::request 发送。
#[derive(Debug, Clone, Copy)]
pub enum DecorationAction {
    Request(WindowRequest),
    Close,
    /// 指针移到了需要不同光标的位置（窗口边框上的调整大小箭头）；调用者交给 [`crate::Display::set_cursor_shape`]。
    Cursor(CursorShape),
}

/// 一个窗口的装饰输入状态；保存局部指针位置和按住的标题按钮，不修改 compositor 配置。
#[derive(Debug)]
pub struct Decoration {
    kind: FrameKind,
    pointer: Option<(i32, i32)>,
    /// 按下时锁定按钮，释放仍在同一按钮才激活；缺少它会在拖入按钮后误触关闭。
    armed: Option<CaptionButton>,
    /// 标题栏左侧的程序图标；缺少它标题栏左端留着空位，与原版不一致。
    icon: Option<std::rc::Rc<Bitmap>>,
    /// 上一次单击标题栏图标的时刻；两次单击间隔在 [`DOUBLE_CLICK`] 内视为双击关闭窗口，缺少它双击图标只会重复弹出菜单。
    icon_click: Option<Instant>,
    /// 已经通知过调用者的光标形状；缺少它每个指针移动事件都会重复请求同一形状。
    shape: CursorShape,
}

impl Decoration {
    /// 建立指定边框类型的装饰控制器。
    pub fn new(kind: FrameKind) -> Self {
        Self {
            kind,
            pointer: None,
            armed: None,
            icon: None,
            icon_click: None,
            shape: CursorShape::Default,
        }
    }

    /// 为尚未迁移共享运行库的外壳 layer 对话框读取标题图标；普通 SDK 窗口使用 Resources 的 Rc。
    pub fn load_icon(&mut self, stem: &str) {
        let path = std::path::Path::new("/usr/share/liteos/ui").join(format!("{stem}-16.bmp"));
        self.icon = Bitmap::load(&path).ok().map(std::rc::Rc::new);
    }

    /// 设置资源上下文提供的共享图标；替换时只释放本装饰的引用。
    pub fn set_icon(&mut self, icon: Option<std::rc::Rc<Bitmap>>) {
        self.icon = icon;
    }

    fn caption(&self, configuration: Configuration) -> Rect {
        let border = self.kind.border();
        Rect::new(
            border,
            border,
            (configuration.width - 2 * border).max(0),
            CAPTION_HEIGHT,
        )
    }

    /// 标题栏左侧的 16×16 图标位（系统菜单按钮）。
    fn icon_rect(&self, configuration: Configuration) -> Rect {
        let caption = self.caption(configuration);
        Rect::new(
            caption.x + 2,
            caption.y + (caption.height - 1 - 16) / 2,
            16,
            16,
        )
    }

    fn buttons(&self, configuration: Configuration) -> Vec<(CaptionButton, bool)> {
        if self.kind == FrameKind::Fixed {
            vec![(CaptionButton::Close, true)]
        } else {
            vec![
                (CaptionButton::Minimize, true),
                (
                    if configuration.maximized {
                        CaptionButton::Restore
                    } else {
                        CaptionButton::Maximize
                    },
                    true,
                ),
                (CaptionButton::Close, true),
            ]
        }
    }

    /// 窗口客户区（逻辑像素）；全屏时整个 surface 都属于客户区。
    pub fn client_rect(&self, configuration: Configuration) -> Rect {
        if configuration.fullscreen {
            return Rect::new(0, 0, configuration.width, configuration.height);
        }
        let inner =
            Rect::new(0, 0, configuration.width, configuration.height).inset(self.kind.border());
        Rect::new(
            inner.x,
            inner.y + CAPTION_HEIGHT,
            inner.width.max(0),
            (inner.height - CAPTION_HEIGHT).max(0),
        )
    }

    /// 当前实际画成按下状态的标题按钮；仅供窗口绘制 owner 比较输入前后视觉变化。
    pub(crate) fn pressed_button(&self, configuration: Configuration) -> Option<CaptionButton> {
        self.armed
            .filter(|button| self.button_at(configuration) == Some(*button))
    }

    /// 绘制外框、标题栏和按钮；返回客户区。configuration 来自已确认的服务端配置。
    pub fn paint(
        &self,
        painter: &mut Painter<'_>,
        configuration: Configuration,
        title: &str,
    ) -> Rect {
        if !configuration.fullscreen {
            painter.window_frame(
                Rect::new(0, 0, configuration.width, configuration.height),
                self.kind,
            );
            let pressed = self.pressed_button(configuration);
            painter.caption_bar(
                self.caption(configuration),
                title,
                self.icon.as_deref(),
                configuration.active,
                &self.buttons(configuration),
                pressed,
            );
        }
        self.client_rect(configuration)
    }

    fn button_at(&self, configuration: Configuration) -> Option<CaptionButton> {
        let point = self.pointer?;
        caption_button_rects(self.caption(configuration), &self.buttons(configuration))
            .into_iter()
            .find(|(_, enabled, rect)| *enabled && contains(*rect, point))
            .map(|(button, _, _)| button)
    }

    /// 指针当前位置应显示的光标：可缩放窗口的边框上是对应方向的调整大小箭头，其余是默认箭头。
    fn shape_at(&self, configuration: Configuration) -> CursorShape {
        let Some(point) = self.pointer else {
            return CursorShape::Default;
        };
        if self.kind != FrameKind::Resizable || configuration.maximized || configuration.fullscreen
        {
            return CursorShape::Default;
        }
        match resize_edge(configuration, point, self.kind.border()) {
            Some(ResizeEdge::Top | ResizeEdge::Bottom) => CursorShape::NsResize,
            Some(ResizeEdge::Left | ResizeEdge::Right) => CursorShape::EwResize,
            Some(ResizeEdge::TopLeft | ResizeEdge::BottomRight) => CursorShape::NwseResize,
            Some(ResizeEdge::TopRight | ResizeEdge::BottomLeft) => CursorShape::NeswResize,
            _ => CursorShape::Default,
        }
    }

    /// 消费 surface 事件并返回移动、缩放、标题按钮操作与光标变化；按钮只在左键释放时激活。
    pub fn event(
        &mut self,
        event: &EventKind,
        configuration: Configuration,
    ) -> Option<DecorationAction> {
        let action = self.act(event, configuration);
        if matches!(event, EventKind::PointerLeave) {
            // 指针离开后 compositor 把光标复位；下次进入窗口时由 enter 之后的判定重新设置。
            self.shape = CursorShape::Default;
        }
        action.or_else(|| {
            let shape = self.shape_at(configuration);
            (std::mem::replace(&mut self.shape, shape) != shape)
                .then_some(DecorationAction::Cursor(shape))
        })
    }

    fn act(&mut self, event: &EventKind, configuration: Configuration) -> Option<DecorationAction> {
        match event {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = Some((x.floor() as i32, y.floor() as i32))
            }
            EventKind::PointerLeave => {
                self.pointer = None;
                self.armed = None;
            }
            EventKind::Focus(false) => self.armed = None,
            EventKind::Button {
                code: 0x111,
                pressed: true,
                serial,
            } if !configuration.fullscreen => {
                let point = self.pointer?;
                if contains(self.caption(configuration), point) {
                    return Some(DecorationAction::Request(WindowRequest::ShowMenu {
                        serial: *serial,
                        x: point.0,
                        y: point.1,
                    }));
                }
            }
            EventKind::Button {
                code: 0x110,
                pressed,
                serial,
            } if !configuration.fullscreen => {
                if *pressed {
                    self.armed = self.button_at(configuration);
                    if self.armed.is_some() {
                        return None;
                    }
                    let icon = self.icon_rect(configuration);
                    if self.pointer.is_some_and(|point| contains(icon, point)) {
                        let double = self
                            .icon_click
                            .take()
                            .is_some_and(|at| at.elapsed() < DOUBLE_CLICK);
                        if double {
                            return Some(DecorationAction::Close);
                        }
                        self.icon_click = Some(Instant::now());
                        return Some(DecorationAction::Request(WindowRequest::ShowMenu {
                            serial: *serial,
                            x: icon.x,
                            y: icon.bottom(),
                        }));
                    }
                    if let Some(point) = self.pointer {
                        if self.kind == FrameKind::Resizable
                            && !configuration.maximized
                            && let Some(edges) =
                                resize_edge(configuration, point, self.kind.border())
                        {
                            return Some(DecorationAction::Request(WindowRequest::Resize {
                                serial: *serial,
                                edges,
                            }));
                        }
                        if !configuration.maximized && contains(self.caption(configuration), point)
                        {
                            return Some(DecorationAction::Request(WindowRequest::Move {
                                serial: *serial,
                            }));
                        }
                    }
                } else if let Some(button) = self
                    .armed
                    .take()
                    .filter(|button| self.button_at(configuration) == Some(*button))
                {
                    return Some(match button {
                        CaptionButton::Close => DecorationAction::Close,
                        CaptionButton::Minimize => {
                            DecorationAction::Request(WindowRequest::Minimize)
                        }
                        CaptionButton::Maximize => {
                            DecorationAction::Request(WindowRequest::Maximize(true))
                        }
                        CaptionButton::Restore => {
                            DecorationAction::Request(WindowRequest::Maximize(false))
                        }
                    });
                }
            }
            _ => {}
        }
        None
    }
}

/// 双击的最大间隔。
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

fn contains(rect: Rect, (x, y): (i32, i32)) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

/// 角区沿边向内延伸的长度：只在 4 像素的角点上才能抓到角太难，Windows 的调整大小框角区也是这样放宽的。
const CORNER: i32 = 16;

/// 指针 `(x, y)` 在窗口边框上抓住的是哪条边或哪个角；不在边框上返回 `None`。
fn resize_edge(
    configuration: Configuration,
    (x, y): (i32, i32),
    border: i32,
) -> Option<ResizeEdge> {
    let (width, height) = (configuration.width, configuration.height);
    if x < 0 || y < 0 || x >= width || y >= height {
        return None;
    }
    let (near_left, near_right) = (x < border, x >= width - border);
    let (near_top, near_bottom) = (y < border, y >= height - border);
    if !(near_left || near_right || near_top || near_bottom) {
        return None;
    }
    // 左右边框靠近上下端、上下边框靠近左右端时算角。
    let (left, right) = (near_left || x < CORNER, near_right || x >= width - CORNER);
    let (top, bottom) = (near_top || y < CORNER, near_bottom || y >= height - CORNER);
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (_, true, true, _) => ResizeEdge::TopRight,
        (true, _, _, true) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        _ if near_left => ResizeEdge::Left,
        _ if near_right => ResizeEdge::Right,
        _ if near_top => ResizeEdge::Top,
        _ => ResizeEdge::Bottom,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn configuration() -> Configuration {
        Configuration {
            width: 320,
            height: 200,
            scale: 1,
            active: true,
            maximized: false,
            fullscreen: false,
            resizing: false,
        }
    }
    fn button(pressed: bool) -> EventKind {
        EventKind::Button {
            code: 0x110,
            pressed,
            serial: 17,
        }
    }
    #[test]
    fn title_drag_and_corner_resize_carry_the_actual_button_serial() {
        let mut decoration = Decoration::new(FrameKind::Resizable);
        let configuration = configuration();
        decoration.event(&EventKind::PointerEnter { x: 50.0, y: 10.0 }, configuration);
        assert!(matches!(
            decoration.event(&button(true), configuration),
            Some(DecorationAction::Request(WindowRequest::Move {
                serial: 17
            }))
        ));
        decoration.event(
            &EventKind::PointerMove { x: 319.0, y: 199.0 },
            configuration,
        );
        assert!(matches!(
            decoration.event(&button(true), configuration),
            Some(DecorationAction::Request(WindowRequest::Resize {
                serial: 17,
                edges: ResizeEdge::BottomRight
            }))
        ));
        // 最大化禁止发起 resize，但必须撤销旧的边框光标。
        assert!(matches!(
            decoration.event(
                &button(true),
                Configuration {
                    maximized: true,
                    ..configuration
                }
            ),
            Some(DecorationAction::Cursor(CursorShape::Default))
        ));
    }
    #[test]
    fn close_requires_press_and_release_on_the_same_button_and_focus_loss_cancels_it() {
        let mut decoration = Decoration::new(FrameKind::Resizable);
        let configuration = configuration();
        decoration.event(
            &EventKind::PointerEnter { x: 305.0, y: 10.0 },
            configuration,
        );
        assert!(decoration.event(&button(false), configuration).is_none());
        decoration.event(&button(true), configuration);
        decoration.event(&EventKind::PointerLeave, configuration);
        decoration.event(
            &EventKind::PointerEnter { x: 305.0, y: 10.0 },
            configuration,
        );
        assert!(decoration.event(&button(false), configuration).is_none());
        decoration.event(&button(true), configuration);
        assert!(matches!(
            decoration.event(&button(false), configuration),
            Some(DecorationAction::Close)
        ));
        // 键盘失焦不等于指针离开：返回焦点后，不移动鼠标也应能再次点击。
        decoration.event(&button(true), configuration);
        decoration.event(&EventKind::Focus(false), configuration);
        assert!(decoration.event(&button(false), configuration).is_none());
        decoration.event(&button(true), configuration);
        assert!(matches!(
            decoration.event(&button(false), configuration),
            Some(DecorationAction::Close)
        ));

        assert_eq!(decoration.client_rect(configuration).y, 23);
        assert_eq!(
            decoration
                .client_rect(Configuration {
                    fullscreen: true,
                    ..configuration
                })
                .y,
            0
        );
    }
}
