//! 外壳对话框；窗口的装饰、configure 与帧仍由 classic-ui 拥有。
use canvas::Rect;
use classic_ui::{
    Configuration, Decoration, DecorationAction, Display, Edit, Event, EventKind, FrameKind,
    KeyboardInteractivity, Layer, LayerSettings, WindowId, WindowRequest, layer_anchor,
};
use std::io;
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Run,
}
pub struct Dialog {
    pub window: WindowId,
    pub kind: Kind,
    pub decoration: Decoration,
    pub edit: Edit,
    pointer: (f64, f64),
    armed: Option<usize>,
    /// 与屏幕左、下边的距离（逻辑像素）；拖动标题栏时由本对话框更新并重新提交 layer 配置。
    margin: (i32, i32),
    /// 正在拖动时按下标题栏的位置（surface 局部坐标）。surface 随指针移动，局部坐标保持不变，
    /// 所以每次移动用“当前局部坐标 - 按下位置”得到的位移就是增量。
    drag: Option<(f64, f64)>,
}
/// 对话框与工作区左、下边缘的初始间距，逻辑像素。
const INITIAL_MARGIN: i32 = 6;
pub enum Action {
    None,
    Close,
    Run(String),
}
impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Run => "运行",
        }
    }
}
impl Dialog {
    /// "运行"是外壳自己的界面，不是应用窗口：用 layer-shell 的锚点与边距定位（左下角、避开任务栏的
    /// 独占区），这是 Wayland 里外壳界面指定位置的标准方式；应用 toplevel 的位置由合成器决定。
    pub fn new(display: &mut Display, kind: Kind) -> io::Result<Self> {
        let size = (440, 210);
        let mut decoration = Decoration::new(FrameKind::Fixed);
        decoration.load_icon("run");
        let margin = (INITIAL_MARGIN, INITIAL_MARGIN);
        Ok(Self {
            window: display.create_layer("org.liteos.run", Self::settings(size, margin))?,
            kind,
            decoration,
            edit: Edit::default(),
            pointer: (0., 0.),
            armed: None,
            margin,
            drag: None,
        })
    }
    fn settings(size: (u32, u32), margin: (i32, i32)) -> LayerSettings {
        LayerSettings {
            layer: Layer::Top,
            width: size.0,
            height: size.1,
            anchor: layer_anchor::LEFT | layer_anchor::BOTTOM,
            margin: [0, 0, margin.1, margin.0],
            // 0：按规范避开任务栏等外壳 surface 的独占区。
            exclusive_zone: 0,
            // 打开后立即可以输入；OnDemand 要先点击才有键盘焦点。
            keyboard: KeyboardInteractivity::Exclusive,
        }
    }
    /// 按指针相对按下位置的位移移动对话框。
    fn drag_to(&mut self, display: &mut Display, c: Configuration) -> io::Result<()> {
        let Some(grab) = self.drag else {
            return Ok(());
        };
        let left = self.margin.0 + (self.pointer.0 - grab.0).round() as i32;
        let bottom = self.margin.1 - (self.pointer.1 - grab.1).round() as i32;
        let margin = (left.max(0), bottom.max(0));
        if margin != self.margin {
            self.margin = margin;
            display.configure_layer(
                self.window,
                Self::settings((c.width as u32, c.height as u32), margin),
            )?;
        }
        Ok(())
    }
    pub fn buttons(&self, c: Configuration) -> Vec<Rect> {
        let y = c.height - 35;
        if self.kind == Kind::Run {
            vec![
                Rect::new(c.width - 175, y, 75, 23),
                Rect::new(c.width - 94, y, 75, 23),
            ]
        } else {
            vec![Rect::new(c.width - 115, y, 100, 23)]
        }
    }
    pub fn event(&mut self, display: &mut Display, event: Event) -> io::Result<Action> {
        let Some(c) = display.configuration(self.window) else {
            return Ok(Action::None);
        };
        match self.decoration.event(&event.kind, c) {
            // layer surface 没有 xdg_toplevel：标题栏拖动由本对话框自己实现。
            Some(DecorationAction::Request(WindowRequest::Move { .. })) => {
                self.drag = Some(self.pointer);
            }
            Some(DecorationAction::Request(_)) => {}
            Some(DecorationAction::Close) => return Ok(Action::Close),
            Some(DecorationAction::Cursor(shape)) => display.set_cursor_shape(shape)?,
            None => {}
        }
        match event.kind {
            EventKind::Close => return Ok(Action::Close),
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = (x, y);
                self.drag_to(display, c)?;
            }
            EventKind::Button {
                code: 0x110,
                pressed,
                ..
            } => {
                let hit = self.buttons(c).iter().position(|r| {
                    self.pointer.0 >= f64::from(r.x)
                        && self.pointer.0 < f64::from(r.right())
                        && self.pointer.1 >= f64::from(r.y)
                        && self.pointer.1 < f64::from(r.bottom())
                });
                if !pressed {
                    self.drag = None;
                }
                if pressed {
                    self.armed = hit;
                } else if let Some(index) = self.armed.take().filter(|i| Some(*i) == hit) {
                    return self.activate(index);
                }
            }
            EventKind::Key(key) if key.pressed => match key.code {
                1 => return Ok(Action::Close),
                28 => return self.activate(0),
                _ if self.kind == Kind::Run && self.edit.text().len() < 4096 => {
                    self.edit.key(key);
                }
                _ => {}
            },
            _ => {}
        }
        Ok(Action::None)
    }
    fn activate(&self, index: usize) -> io::Result<Action> {
        match self.kind {
            Kind::Run if index == 0 && !self.edit.text().trim().is_empty() => {
                Ok(Action::Run(self.edit.text().into()))
            }
            _ => Ok(Action::Close),
        }
    }
}
