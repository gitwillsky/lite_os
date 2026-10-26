//! 属性表默认行为：验证和应用分阶段，异步业务结果必须匹配当前序号。
use super::{
    context::{Context, WindowBody},
    widgets::{Tree, invalid},
};
use classic_ui_abi::{self as abi, event, state, widget};
use std::io;
#[derive(Clone, Copy)]
enum Phase {
    Idle,
    Validating { sequence: u64, close: bool },
    Applying { sequence: u64, close: bool },
}
pub struct Sheet {
    pub tree: Tree,
    pub pages: u64,
    ok: u64,
    cancel: u64,
    apply: u64,
    error: u64,
    dirty: bool,
    next: u64,
    phase: Phase,
}
fn spec(kind: u32, text: &str, layout: abi::Layout) -> (abi::Widget, String) {
    (
        abi::Widget {
            kind,
            state: 0,
            group: 0,
            layout,
            text: abi::Text {
                data: std::ptr::null(),
                len: 0,
            },
        },
        text.into(),
    )
}
pub(super) fn add(
    tree: &mut Tree,
    parent: u64,
    kind: u32,
    text: &str,
    layout: abi::Layout,
) -> io::Result<u64> {
    let (value, text) = spec(kind, text, layout);
    tree.insert(parent, value, text)
}
impl Sheet {
    pub fn new() -> io::Result<Self> {
        let mut tree = Tree::new();
        let root = add(
            &mut tree,
            0,
            widget::COLUMN,
            "",
            abi::Layout {
                grow: 1,
                padding: 8,
                gap: 8,
                ..Default::default()
            },
        )?;
        let pages = add(
            &mut tree,
            root,
            widget::TABS,
            "",
            abi::Layout {
                grow: 1,
                min_height: 100,
                ..Default::default()
            },
        )?;
        let error = add(
            &mut tree,
            root,
            widget::LABEL,
            "",
            abi::Layout {
                height: 40,
                ..Default::default()
            },
        )?;
        tree.nodes.get_mut(&error).expect("error node").state = state::HIDDEN | state::WRAP;
        let buttons = add(
            &mut tree,
            root,
            widget::ROW,
            "",
            abi::Layout {
                height: 23,
                gap: 6,
                ..Default::default()
            },
        )?;
        add(
            &mut tree,
            buttons,
            widget::LABEL,
            "",
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let button_layout = abi::Layout {
            width: 75,
            height: 23,
            ..Default::default()
        };
        let ok = add(&mut tree, buttons, widget::BUTTON, "确定", button_layout)?;
        let cancel = add(&mut tree, buttons, widget::BUTTON, "取消", button_layout)?;
        let apply = add(&mut tree, buttons, widget::BUTTON, "应用(A)", button_layout)?;
        tree.nodes.get_mut(&ok).expect("default button").state |= state::DEFAULT;
        let mut sheet = Self {
            tree,
            pages,
            ok,
            cancel,
            apply,
            error,
            dirty: false,
            next: 1,
            phase: Phase::Idle,
        };
        sheet.controls();
        Ok(sheet)
    }
    fn controls(&mut self) {
        self.tree.layout_dirty = true;
        let idle = matches!(self.phase, Phase::Idle);
        let applying = matches!(self.phase, Phase::Applying { .. });
        for (id, enabled) in [
            (self.pages, idle),
            (self.ok, idle),
            (self.cancel, !applying),
            (self.apply, idle && self.dirty),
        ] {
            let state = &mut self
                .tree
                .nodes
                .get_mut(&id)
                .expect("sheet built-in control")
                .state;
            if enabled {
                *state &= !state::DISABLED;
            } else {
                *state |= state::DISABLED;
            }
        }
    }
    pub fn set_dirty(&mut self, dirty: bool) -> io::Result<()> {
        if !matches!(self.phase, Phase::Idle) {
            return Err(invalid("property sheet is applying"));
        }
        self.dirty = dirty;
        self.controls();
        Ok(())
    }
    pub fn applying(&self) -> bool {
        matches!(self.phase, Phase::Applying { .. })
    }
    pub fn actions(
        &mut self,
        window: u64,
        actions: Vec<abi::Event>,
        close: bool,
    ) -> io::Result<Vec<abi::Event>> {
        let mut out = Vec::new();
        // 应用阶段的外部副作用已开始：X/Escape/Alt+F4 暂缓到 applied，成功后关闭，
        // 失败保留窗口展示错误。直接丢弃会让用户的关闭请求无声失效。
        if close {
            match &mut self.phase {
                Phase::Applying { close: pending, .. } => *pending = true,
                _ => out.push(abi::Event {
                    window,
                    kind: event::CLOSE,
                    ..Default::default()
                }),
            }
        }
        for action in actions {
            if action.kind == event::ACTIVATE && action.node == self.cancel {
                if !self.applying() {
                    out.push(abi::Event {
                        window,
                        kind: event::CLOSE,
                        ..Default::default()
                    });
                }
            } else if action.kind == event::ACTIVATE
                && (action.node == self.ok || action.node == self.apply)
            {
                if !matches!(self.phase, Phase::Idle) {
                    continue;
                }
                let close = action.node == self.ok;
                if !self.dirty {
                    if close {
                        out.push(abi::Event {
                            window,
                            kind: event::CLOSE,
                            ..Default::default()
                        });
                    }
                    continue;
                }
                let sequence = self.next;
                self.next = sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid("sheet operation identities exhausted"))?;
                self.phase = Phase::Validating { sequence, close };
                self.controls();
                out.push(abi::Event {
                    window,
                    kind: event::SHEET_VALIDATE,
                    value: sequence,
                    ..Default::default()
                });
            } else {
                // 页内数据交互使草稿失效；标签页切换本身不修改设置。
                if action.node != self.pages
                    && self.descendant(action.node, self.pages)
                    && matches!(
                        action.kind,
                        event::CHANGED | event::ACTIVATE | event::SELECT
                    )
                {
                    self.dirty = true;
                    self.controls();
                }
                out.push(action);
            }
        }
        Ok(out)
    }
    fn descendant(&self, mut id: u64, ancestor: u64) -> bool {
        while id != 0 {
            if id == ancestor {
                return true;
            }
            let Some(node) = self.tree.nodes.get(&id) else {
                return false;
            };
            id = node.parent;
        }
        false
    }
    fn failure(&mut self, page: u64, focus: u64, message: String) -> io::Result<()> {
        if page != 0
            && self
                .tree
                .nodes
                .get(&page)
                .is_none_or(|node| node.parent != self.pages)
        {
            return Err(invalid("validation page is not a sheet page"));
        }
        if focus != 0 && !self.descendant(focus, if page == 0 { self.pages } else { page }) {
            return Err(invalid("validation focus is outside the page"));
        }
        if page != 0 {
            self.tree
                .nodes
                .get_mut(&self.pages)
                .expect("sheet pages")
                .selected = Some(page);
        }
        if focus != 0 {
            self.tree.focus = Some(focus);
        }
        let error = self.tree.nodes.get_mut(&self.error).expect("sheet error");
        error.text = message;
        error.state &= !state::HIDDEN;
        self.phase = Phase::Idle;
        self.dirty = true;
        self.controls();
        Ok(())
    }
    pub fn validated(
        &mut self,
        window: u64,
        sequence: u64,
        accepted: bool,
        page: u64,
        focus: u64,
        message: String,
    ) -> io::Result<Option<abi::Event>> {
        let Phase::Validating {
            sequence: current,
            close,
        } = self.phase
        else {
            return Err(invalid("sheet is not validating"));
        };
        if current != sequence {
            return Err(invalid("stale validation result"));
        }
        if !accepted {
            self.failure(page, focus, message)?;
            return Ok(None);
        }
        self.phase = Phase::Applying { sequence, close };
        self.tree
            .nodes
            .get_mut(&self.error)
            .expect("sheet error")
            .state |= state::HIDDEN;
        self.controls();
        Ok(Some(abi::Event {
            window,
            kind: event::SHEET_APPLY,
            value: sequence,
            ..Default::default()
        }))
    }
    pub fn applied(
        &mut self,
        window: u64,
        sequence: u64,
        success: bool,
        message: String,
    ) -> io::Result<Option<abi::Event>> {
        let Phase::Applying {
            sequence: current,
            close,
        } = self.phase
        else {
            return Err(invalid("sheet is not applying"));
        };
        if current != sequence {
            return Err(invalid("stale apply result"));
        }
        if !success {
            self.failure(0, 0, message)?;
            return Ok(None);
        }
        self.phase = Phase::Idle;
        self.dirty = false;
        self.controls();
        Ok(close.then_some(abi::Event {
            window,
            kind: event::CLOSE,
            ..Default::default()
        }))
    }
}
impl Context {
    pub fn property_sheet(
        &mut self,
        parent: u64,
        title: &str,
        width: i32,
        height: i32,
    ) -> io::Result<(u64, u64)> {
        let sheet = Sheet::new()?;
        let pages = sheet.pages;
        let app = if parent == 0 {
            "org.liteos.properties".into()
        } else {
            self.host(parent)?.app_id.clone()
        };
        let id = self.window(title, &app, (width, height), true, parent, parent != 0)?;
        self.host_mut(id)?.body = WindowBody::Sheet(sheet);
        Ok((id, pages))
    }
    pub fn sheet(&mut self, id: u64) -> io::Result<&mut Sheet> {
        match &mut self.host_mut(id)?.body {
            WindowBody::Sheet(sheet) => Ok(sheet),
            _ => Err(invalid("not a property sheet")),
        }
    }
    pub fn publish(&mut self, event: Option<abi::Event>) {
        if let Some(event) = event {
            if matches!(event.kind, event::CHANGED | event::SELECT | event::ACTIVATE)
                && let Some(host) = self.windows.get_mut(&event.window)
            {
                if let WindowBody::Sheet(sheet) = &mut host.body {
                    // 内部异步粘贴与普通输入共享属性表动作路径，否则 Apply 不会启用。
                    if let Ok(actions) = sheet.actions(event.window, vec![event], false) {
                        self.events.extend(actions);
                    }
                    return;
                }
                if let WindowBody::Dialog(dialog) = &mut host.body {
                    let action = dialog.actions(
                        event.window,
                        vec![event],
                        &classic_ui::EventKind::Focus(true),
                        false,
                    );
                    self.file_action(event.window, action);
                    return;
                }
            }
            self.events.push_back(event);
        }
    }
}
