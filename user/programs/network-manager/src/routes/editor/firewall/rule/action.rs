//! 动作仅显示当前选择需要的参数，覆盖同族单向与组合 NAT。

use super::super::super::optional_word;
use super::{InputError, RuleKind, choice_field, field, optional_port, selection};
use crate::child;
use liteos_ui::{Event, EventKind, Layout, Widget, WidgetKind, state};
use network_config::configuration::{ConfigurationName, NatSource, NatTarget, PreroutingAction};

pub(super) struct Fields {
    kind: Widget,
    kinds: &'static [usize],
    mark_panel: Widget,
    mark: Widget,
    jump_panel: Widget,
    jump: Widget,
    primary_panel: Widget,
    primary: (Widget, Widget),
    combined_panel: Widget,
    combined: Widget,
    secondary_panel: Widget,
    secondary: (Widget, Widget),
    chains: Vec<ConfigurationName>,
    _layout: Vec<Widget>,
}

impl Fields {
    pub(super) fn new(
        root: &Widget,
        chains: &[&ConfigurationName],
        current: Option<&PreroutingAction>,
        scope: Option<RuleKind>,
    ) -> liteos_ui::Result<Self> {
        let mut layout = Vec::new();
        let names = [
            "放行",
            "丢弃",
            "返回",
            "设置路由标记",
            "跳转到子链",
            "目的地址转换",
            "固定源地址转换",
            "自动源地址转换",
        ];
        let kinds: &[usize] = match scope {
            Some(RuleKind::Filter) => &[0, 1, 2, 4],
            Some(RuleKind::Nat) => &[5, 6, 7],
            None => &[0, 1, 2, 3, 4, 5, 6, 7],
        };
        let kind = choice_field(
            root,
            &mut layout,
            "动作",
            kinds.iter().map(|&index| names[index]),
        )?;
        let (index, mark, chain, target, source) = match current {
            None => (kinds[0], None, None, None, None),
            Some(PreroutingAction::Accept) => (0, None, None, None, None),
            Some(PreroutingAction::Drop) => (1, None, None, None, None),
            Some(PreroutingAction::Return) => (2, None, None, None, None),
            Some(PreroutingAction::SetMark { value }) => (3, Some(*value), None, None, None),
            Some(PreroutingAction::Jump { chain }) => (4, None, Some(chain), None, None),
            Some(PreroutingAction::DestinationNat {
                address,
                port,
                source,
            }) => (
                5,
                None,
                None,
                Some(NatTarget {
                    address: *address,
                    port: *port,
                }),
                *source,
            ),
            Some(PreroutingAction::SourceNat(value)) => (6, None, None, Some(*value), None),
            Some(PreroutingAction::Masquerade) => (7, None, None, None, None),
        };
        let index = kinds
            .iter()
            .position(|&kind| index == kind)
            .expect("rule action belongs to the pane");
        kind.select(Some(index as u64 + 1))?;
        let mark_panel = panel(root)?;
        let mark = field(
            &mark_panel,
            &mut layout,
            "路由标记",
            WidgetKind::Edit,
            &mark.map_or_else(String::new, |value| format!("{value:#x}")),
            false,
        )?;
        let jump_panel = panel(root)?;
        let jump = choice_field(
            &jump_panel,
            &mut layout,
            "目标子链",
            chains.iter().map(|name| name.as_str()),
        )?;
        if let Some(index) =
            chain.and_then(|name| chains.iter().position(|candidate| *candidate == name))
        {
            jump.select(Some(index as u64 + 1))?;
        }
        let primary_panel = panel(root)?;
        let primary = target_fields(&primary_panel, &mut layout, "转换地址", "转换端口", target)?;
        let combined_panel = panel(root)?;
        let combined = choice_field(
            &combined_panel,
            &mut layout,
            "同时转换源",
            ["保持原源地址", "固定源地址", "使用出口地址"],
        )?;
        let (combined_index, source_target) = match source {
            None => (1, None),
            Some(NatSource::Fixed(target)) => (2, Some(target)),
            Some(NatSource::Masquerade) => (3, None),
        };
        combined.select(Some(combined_index))?;
        let secondary_panel = panel(root)?;
        let secondary = target_fields(
            &secondary_panel,
            &mut layout,
            "源转换地址",
            "源转换端口",
            source_target,
        )?;
        let fields = Self {
            kind,
            kinds,
            mark_panel,
            mark,
            jump_panel,
            jump,
            primary_panel,
            primary,
            combined_panel,
            combined,
            secondary_panel,
            secondary,
            chains: chains.iter().map(|name| (*name).clone()).collect(),
            _layout: layout,
        };
        fields.refresh()?;
        Ok(fields)
    }

    fn selected(&self) -> liteos_ui::Result<usize> {
        Ok(self.kinds[selection(&self.kind)?.unwrap_or(0)])
    }

    fn refresh(&self) -> liteos_ui::Result<()> {
        let action = self.selected()?;
        let visible = |panel: &Widget, show| panel.set_state(if show { 0 } else { state::HIDDEN });
        visible(&self.mark_panel, action == 3)?;
        visible(&self.jump_panel, action == 4)?;
        visible(&self.primary_panel, matches!(action, 5 | 6))?;
        visible(&self.combined_panel, action == 5)?;
        visible(
            &self.secondary_panel,
            action == 5 && selection(&self.combined)? == Some(1),
        )
    }

    pub(super) fn handle(&self, event: &Event) -> liteos_ui::Result<()> {
        if matches!(event.kind, EventKind::Select { .. })
            && (event.node == self.kind.id() || event.node == self.combined.id())
        {
            self.refresh()?;
        }
        Ok(())
    }

    pub(super) fn value(&self) -> Result<PreroutingAction, InputError> {
        Ok(match self.selected()? {
            0 => PreroutingAction::Accept,
            1 => PreroutingAction::Drop,
            2 => PreroutingAction::Return,
            3 => {
                let value = optional_word(&self.mark.text()?)
                    .ok()
                    .flatten()
                    .ok_or("路由标记须为 32 位无符号整数。")?;
                PreroutingAction::SetMark { value }
            }
            4 => {
                let chain = selection(&self.jump)?
                    .and_then(|index| self.chains.get(index))
                    .ok_or("请选择目标子链。")?;
                PreroutingAction::Jump {
                    chain: chain.clone(),
                }
            }
            action @ (5 | 6) => {
                let target = read_target(&self.primary)?
                    .ok_or("请输入转换地址。端口可留空或填 1–65535。")?;
                if action == 6 {
                    PreroutingAction::SourceNat(target)
                } else {
                    let source = match selection(&self.combined)? {
                        Some(1) => Some(NatSource::Fixed(
                            read_target(&self.secondary)?
                                .ok_or("请输入有效的源转换地址与可选端口。")?,
                        )),
                        Some(2) => Some(NatSource::Masquerade),
                        _ => None,
                    };
                    PreroutingAction::DestinationNat {
                        address: target.address,
                        port: target.port,
                        source,
                    }
                }
            }
            _ => PreroutingAction::Masquerade,
        })
    }
}

fn panel(parent: &Widget) -> liteos_ui::Result<Widget> {
    child(
        parent,
        WidgetKind::Column,
        "",
        Layout {
            gap: 6,
            ..Layout::default()
        },
    )
}

fn target_fields(
    parent: &Widget,
    layout: &mut Vec<Widget>,
    address: &str,
    port: &str,
    current: Option<NatTarget>,
) -> liteos_ui::Result<(Widget, Widget)> {
    let address = field(
        parent,
        layout,
        address,
        WidgetKind::Edit,
        &current.map_or_else(String::new, |value| value.address.to_string()),
        false,
    )?;
    let port = field(
        parent,
        layout,
        port,
        WidgetKind::Edit,
        &current
            .and_then(|value| value.port)
            .map_or_else(String::new, |value| value.to_string()),
        false,
    )?;
    Ok((address, port))
}

fn read_target((address, port): &(Widget, Widget)) -> liteos_ui::Result<Option<NatTarget>> {
    let address = address.text()?.trim().parse().ok();
    let port = optional_port(&port.text()?).ok();
    Ok(address
        .zip(port)
        .map(|(address, port)| NatTarget { address, port }))
}
