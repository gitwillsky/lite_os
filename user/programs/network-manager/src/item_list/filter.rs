//! WinBox 列表筛选控制。各列表定义字段及其谓词，应用与停用共用同一草稿生命周期。

use super::{Glyph, Item, Tool, toolbar};
use crate::child;
use liteos_ui::{Event, EventKind, Layout, TreeRow, Widget, WidgetKind, state};

const ROW_HEIGHT: i32 = 25;
const VISIBLE_ROWS: usize = 4;
const MATCH: u64 = 1;
const EXCLUDE: u64 = 2;
type Parsed<C> = Result<Vec<C>, (usize, &'static str)>;

/// 可筛选字段及其已校验的条件。调用者按自己的数据来源执行条件。
pub(crate) trait Field: Copy + 'static {
    type Condition: PartialEq;

    fn label(self) -> &'static str;

    fn modes(self) -> (&'static str, &'static str) {
        ("=", "≠")
    }

    /// 解析非空操作数；错误文本用于标记无法应用的条件。
    fn parse(self, exclude: bool, text: &str) -> Result<Self::Condition, &'static str>;
}

/// 筛选区请求更换条件、报告输入错误，或维持当前条件。
pub(crate) enum Action<C> {
    Apply(Vec<C>),
    Invalid(String),
    None,
}

pub(crate) struct Panel<F: Field> {
    fields: &'static [F],
    toggle: Tool,
    panel: Widget,
    rows: Widget,
    apply: Widget,
    drafts: Vec<Draft>,
}

impl<F: Field> Panel<F> {
    /// 在工具栏右侧放置入口，在工具栏和列表之间放置默认收起的条件区。字段列表须非空。
    ///
    /// 错误：窗口已关闭或控件拒绝初始化。
    pub(crate) fn new(
        parent: &Widget,
        bar: &Widget,
        fields: &'static [F],
    ) -> liteos_ui::Result<Self> {
        let [toggle] = toolbar(bar, [Item::Command(Glyph::Filter, "筛选")])?;
        let panel = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 4,
                ..Layout::default()
            },
        )?;
        let rows = child(
            &panel,
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        rows.set_state(state::VSCROLL)?;
        let apply = child(
            &panel,
            WidgetKind::Button,
            "应用",
            Layout {
                width: 74,
                height: ROW_HEIGHT,
                ..Layout::default()
            },
        )?;
        panel.set_state(state::HIDDEN)?;
        let mut filter = Self {
            fields,
            toggle,
            panel,
            rows,
            apply,
            drafts: Vec::new(),
        };
        filter.add()?;
        Ok(filter)
    }

    /// 切换条件区、编辑草稿或请求应用。收起筛选区时停用筛选，保留草稿。
    ///
    /// 错误：窗口已关闭或无法读取控件值。
    pub(crate) fn handle(
        &mut self,
        event: &Event,
        applied: &[F::Condition],
    ) -> liteos_ui::Result<Action<F::Condition>> {
        if !matches!(
            event.kind,
            EventKind::Activate { .. } | EventKind::Select { .. } | EventKind::Changed { .. }
        ) {
            return Ok(Action::None);
        }
        if self.toggle.activated(event) {
            if self.panel.state()? & state::HIDDEN == 0 {
                self.panel.set_state(state::HIDDEN)?;
                self.apply.set_text("应用")?;
                return Ok(if applied.is_empty() {
                    Action::None
                } else {
                    Action::Apply(Vec::new())
                });
            }
            self.panel.set_state(0)?;
            return Ok(Action::None);
        }
        if self.panel.state()? & state::HIDDEN != 0 {
            return Ok(Action::None);
        }
        if matches!(event.kind, EventKind::Activate { .. }) {
            if event.node == self.apply.id() {
                return match self.conditions()? {
                    Ok(next) => {
                        let next = if next == applied { Vec::new() } else { next };
                        self.apply
                            .set_text(if next.is_empty() { "应用" } else { "停用" })?;
                        Ok(Action::Apply(next))
                    }
                    Err((index, message)) => {
                        self.drafts[index].value.focus()?;
                        Ok(Action::Invalid(format!(
                            "第 {} 条条件：{message}",
                            index + 1
                        )))
                    }
                };
            }
            if let Some(index) = self
                .drafts
                .iter()
                .position(|draft| draft.remove.id() == event.node)
            {
                if self.drafts.len() == 1 {
                    self.drafts[0].value.set_text("")?;
                } else {
                    self.drafts.remove(index);
                    self.resize()?;
                }
                self.update_button(applied)?;
            } else if self.drafts.iter().any(|draft| draft.add.id() == event.node) {
                self.add()?;
                self.drafts.last().expect("added draft").value.focus()?;
                self.update_button(applied)?;
            }
        } else if matches!(
            event.kind,
            EventKind::Select { .. } | EventKind::Changed { .. }
        ) && let Some(draft) = self.drafts.iter().find(|draft| {
            [draft.field.id(), draft.mode.id(), draft.value.id()].contains(&event.node)
        }) {
            if event.node == draft.field.id() {
                draft.update_mode(self.fields)?;
            }
            self.update_button(applied)?;
        }
        Ok(Action::None)
    }

    fn conditions(&self) -> liteos_ui::Result<Parsed<F::Condition>> {
        let mut conditions = Vec::new();
        for (index, draft) in self.drafts.iter().enumerate() {
            let input = draft.value.text()?;
            if input.trim().is_empty() {
                continue;
            }
            match draft
                .field(self.fields)?
                .parse(draft.mode.selection()? == Some(EXCLUDE), &input)
            {
                Ok(condition) => conditions.push(condition),
                Err(message) => return Ok(Err((index, message))),
            }
        }
        Ok(Ok(conditions))
    }

    fn update_button(&self, applied: &[F::Condition]) -> liteos_ui::Result<()> {
        let unchanged = !applied.is_empty()
            && self
                .conditions()?
                .is_ok_and(|conditions| conditions == applied);
        self.apply.set_text(if unchanged { "停用" } else { "应用" })
    }

    fn add(&mut self) -> liteos_ui::Result<()> {
        self.drafts.push(Draft::new(&self.rows, self.fields)?);
        self.resize()
    }

    fn resize(&self) -> liteos_ui::Result<()> {
        self.rows.set_layout(Layout {
            grow: 1,
            height: self.drafts.len().min(VISIBLE_ROWS) as i32 * ROW_HEIGHT,
            gap: 0,
            ..Layout::default()
        })
    }
}

struct Draft {
    field: Widget,
    mode: Widget,
    value: Widget,
    add: Widget,
    remove: Widget,
    _row: Widget,
}

impl Draft {
    fn new<F: Field>(parent: &Widget, fields: &[F]) -> liteos_ui::Result<Self> {
        let fixed = |width| Layout {
            width,
            height: ROW_HEIGHT,
            ..Layout::default()
        };
        let row = child(parent, WidgetKind::Row, "", fixed(-1))?;
        let field = child(&row, WidgetKind::Combo, "", fixed(135))?;
        field.set_state(state::DROPDOWN_LIST)?;
        let choices: Vec<_> = fields
            .iter()
            .enumerate()
            .map(|(index, field)| option(index as u64 + 1, field.label()))
            .collect();
        field.set_tree_rows(&choices)?;
        field.select(Some(1))?;
        let mode = child(&row, WidgetKind::Combo, "", fixed(74))?;
        mode.set_state(state::DROPDOWN_LIST)?;
        let value = child(
            &row,
            WidgetKind::Edit,
            "",
            Layout {
                grow: 1,
                height: ROW_HEIGHT,
                ..Layout::default()
            },
        )?;
        let add = child(&row, WidgetKind::Button, "+", fixed(24))?;
        let remove = child(&row, WidgetKind::Button, "−", fixed(24))?;
        let draft = Self {
            field,
            mode,
            value,
            add,
            remove,
            _row: row,
        };
        draft.update_mode(fields)?;
        Ok(draft)
    }

    fn field<F: Field>(&self, fields: &[F]) -> liteos_ui::Result<F> {
        Ok(fields[(self.field.selection()?.unwrap_or(1) - 1) as usize])
    }

    fn update_mode(&self, fields: &[impl Field]) -> liteos_ui::Result<()> {
        let previous = self.mode.selection()?.unwrap_or(MATCH);
        let (include, exclude) = self.field(fields)?.modes();
        self.mode
            .set_tree_rows(&[option(MATCH, include), option(EXCLUDE, exclude)])?;
        self.mode.select(Some(previous))
    }
}

fn option(id: u64, text: &str) -> TreeRow<'_> {
    TreeRow {
        id,
        parent: 0,
        text,
        expanded: false,
        has_children: false,
    }
}
