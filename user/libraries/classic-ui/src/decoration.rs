//! 客户端装饰的命中与输入策略；几何和按钮位置与 Painter 共用同一份事实。

use crate::wayland::ResizeEdge;
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
}

/// 一个窗口的装饰输入状态；保存局部指针位置和按住的标题按钮，不修改 compositor 配置。
#[derive(Debug)]
pub struct Decoration {
    kind: FrameKind,
    pointer: Option<(i32, i32)>,
    /// 按下时锁定按钮，释放仍在同一按钮才激活；缺少它会在拖入按钮后误触关闭。
    armed: Option<CaptionButton>,
}

impl Decoration {
    /// 建立指定边框类型的装饰控制器。
    pub fn new(kind: FrameKind) -> Self {
        Self {
            kind,
            pointer: None,
            armed: None,
        }
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
            let pressed = self
                .armed
                .filter(|button| self.button_at(configuration) == Some(*button));
            painter.caption_bar(
                self.caption(configuration),
                title,
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

    /// 消费 surface 事件并返回移动、缩放、标题按钮操作；按钮只在左键释放时激活。
    pub fn event(
        &mut self,
        event: &EventKind,
        configuration: Configuration,
    ) -> Option<DecorationAction> {
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
                code: 0x110,
                pressed,
                serial,
            } if !configuration.fullscreen => {
                if *pressed {
                    self.armed = self.button_at(configuration);
                    if self.armed.is_some() {
                        return None;
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

fn contains(rect: Rect, (x, y): (i32, i32)) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

fn resize_edge(
    configuration: Configuration,
    (x, y): (i32, i32),
    border: i32,
) -> Option<ResizeEdge> {
    if x < 0 || y < 0 || x >= configuration.width || y >= configuration.height {
        return None;
    }
    let (left, right, top, bottom) = (
        x < border,
        x >= configuration.width - border,
        y < border,
        y >= configuration.height - border,
    );
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (_, true, true, _) => ResizeEdge::TopRight,
        (true, _, _, true) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (true, _, _, _) => ResizeEdge::Left,
        (_, true, _, _) => ResizeEdge::Right,
        (_, _, true, _) => ResizeEdge::Top,
        (_, _, _, true) => ResizeEdge::Bottom,
        _ => return None,
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
        assert!(
            decoration
                .event(
                    &button(true),
                    Configuration {
                        maximized: true,
                        ..configuration
                    }
                )
                .is_none()
        );
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
