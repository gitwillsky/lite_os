//! 共用 MenuBar 状态机，同时接受静态系统菜单与 ABI 复制后的动态菜单。
use super::widgets::Tree;
use classic_ui::{DropMenu, EventKind, MenuBar, MenuEvent, Painter};
use classic_ui_abi::{self as abi, event};
pub struct MenuModel {
    pub entries: Vec<DropMenu<u64, String>>,
    pub state: MenuBar,
}
impl MenuModel {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            state: MenuBar::new(),
        }
    }
}
impl Tree {
    pub fn menu_input(
        &mut self,
        window: u64,
        input: &EventKind,
        probe: &Painter<'_>,
        out: &mut Vec<abi::Event>,
    ) -> (bool, bool) {
        if let EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } = input {
            self.pointer = (x.floor() as i32, y.floor() as i32);
        }
        let popup = self.popup.filter(|id| {
            self.nodes.get(id).is_some_and(|node| {
                node.visible()
                    && node.enabled()
                    && node.menu.as_ref().is_some_and(|menu| menu.state.active())
            })
        });
        let Some(id) = popup.or_else(|| self.menubar.filter(|id| self.active(*id))) else {
            return (false, false);
        };
        let node = self.nodes.get_mut(&id).expect("menu node");
        let menu = node.menu.as_mut().expect("menu model");
        let before = menu.state.visual();
        let was_active = menu.state.active();
        let result = match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => menu
                .state
                .press(probe, node.rect, &menu.entries, self.pointer),
            EventKind::PointerEnter { .. } | EventKind::PointerMove { .. } => {
                if menu.state.active() {
                    menu.state
                        .motion(probe, node.rect, &menu.entries, self.pointer);
                    MenuEvent::Handled
                } else {
                    MenuEvent::Ignored
                }
            }
            EventKind::Key(key) if key.pressed => {
                menu.state.key(probe, node.rect, &menu.entries, key)
            }
            EventKind::Key(key) => {
                if menu.state.key_released(key) {
                    MenuEvent::Handled
                } else {
                    MenuEvent::Ignored
                }
            }
            EventKind::Focus(false) => {
                menu.state.close();
                MenuEvent::Ignored
            }
            _ => MenuEvent::Ignored,
        };
        let repaint = before != menu.state.visual();
        if popup.is_some()
            && was_active
            && !menu.state.active()
            && !matches!(result, MenuEvent::Command(_))
        {
            out.push(abi::Event {
                window,
                node: id,
                kind: event::DISMISS,
                ..Default::default()
            });
        }
        match result {
            MenuEvent::Ignored => (false, repaint),
            MenuEvent::Handled => {
                self.armed = None;
                (true, repaint)
            }
            MenuEvent::Command(command) => {
                self.armed = None;
                out.push(abi::Event {
                    window,
                    node: id,
                    kind: event::ACTIVATE,
                    value: command,
                    ..Default::default()
                });
                (true, repaint)
            }
        }
    }
}
