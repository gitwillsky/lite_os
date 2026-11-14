//! 动作仅显示当前选择需要的参数，覆盖同族单向与组合 NAT。

use super::super::super::optional_word;
use super::{choice_field, field, form, form_buttons, optional_port, selection};
use crate::child;
use liteos_ui::{Dialogs, EventKind, Layout, Widget, WidgetKind, Window, state};
use network_config::configuration::{ConfigurationName, NatSource, NatTarget, PreroutingAction};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    chains: &[&ConfigurationName],
    current: &PreroutingAction,
) -> liteos_ui::Result<Option<PreroutingAction>> {
    let (_window, mut events, root) = form(dialogs, parent, "防火墙动作", 520, 335)?;
    let kind = choice_field(
        &root,
        "动作",
        [
            "放行",
            "丢弃",
            "返回",
            "设置路由标记",
            "跳转到子链",
            "目的地址转换",
            "固定源地址转换",
            "自动源地址转换",
        ],
    )?;
    let (index, mark, chain, target, source) = match *current {
        PreroutingAction::Accept => (0, None, None, None, None),
        PreroutingAction::Drop => (1, None, None, None, None),
        PreroutingAction::Return => (2, None, None, None, None),
        PreroutingAction::SetMark { value } => (3, Some(value), None, None, None),
        PreroutingAction::Jump { ref chain } => (4, None, Some(chain), None, None),
        PreroutingAction::DestinationNat {
            address,
            port,
            source,
        } => (5, None, None, Some(NatTarget { address, port }), source),
        PreroutingAction::SourceNat(value) => (6, None, None, Some(value), None),
        PreroutingAction::Masquerade => (7, None, None, None, None),
    };
    kind.select(Some(index + 1))?;
    let mark_panel = panel(&root)?;
    let (_, mark) = field(
        &mark_panel,
        "路由标记",
        &mark.map_or_else(String::new, |value| format!("{value:#x}")),
    )?;
    let jump_panel = panel(&root)?;
    let jump = choice_field(
        &jump_panel,
        "目标子链",
        chains.iter().map(|name| name.as_str()),
    )?;
    if let Some(index) =
        chain.and_then(|name| chains.iter().position(|candidate| *candidate == name))
    {
        jump.select(Some(index as u64 + 1))?;
    }
    let primary_panel = panel(&root)?;
    let primary = target_fields(&primary_panel, "转换地址", "转换端口", target)?;
    let combined_panel = panel(&root)?;
    let combined = choice_field(
        &combined_panel,
        "同时转换源",
        ["保持原源地址", "固定源地址", "使用出口地址"],
    )?;
    let (combined_index, source_target) = match source {
        None => (1, None),
        Some(NatSource::Fixed(target)) => (2, Some(target)),
        Some(NatSource::Masquerade) => (3, None),
    };
    combined.select(Some(combined_index))?;
    let secondary_panel = panel(&root)?;
    let secondary = target_fields(&secondary_panel, "源转换地址", "源转换端口", source_target)?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;
    let refresh = || {
        let action = selection(&kind)?.unwrap_or(0);
        let set_visible =
            |panel: &Widget, visible| panel.set_state(if visible { 0 } else { state::HIDDEN });
        set_visible(&mark_panel, action == 3)?;
        set_visible(&jump_panel, action == 4)?;
        set_visible(&primary_panel, matches!(action, 5 | 6))?;
        set_visible(&combined_panel, action == 5)?;
        set_visible(
            &secondary_panel,
            action == 5 && selection(&combined)? == Some(1),
        )?;
        let hint = match action {
            2 => "结束当前链。子链返回后继续匹配调用规则的下一条。",
            3 => "支持十进制和 0x 前缀。设置标记后继续匹配。",
            4 if chains.is_empty() => "请先在防火墙设置中创建可跳转的子链。",
            5 | 6 => "地址须与连接同族。端口留空时保持原端口。",
            7 => "使用实际出口地址，必要时自动选择源端口。",
            _ => "",
        };
        status.set_text(hint)
    };
    refresh()?;

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Select { .. } if event.node == kind.id() || event.node == combined.id() => {
                refresh()?
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let result = match selection(&kind)?.unwrap_or(0) {
                    0 => PreroutingAction::Accept,
                    1 => PreroutingAction::Drop,
                    2 => PreroutingAction::Return,
                    3 => {
                        let Ok(Some(value)) = optional_word(&mark.text()?) else {
                            status.set_text("路由标记须为 32 位无符号整数。")?;
                            mark.focus()?;
                            continue;
                        };
                        PreroutingAction::SetMark { value }
                    }
                    4 => {
                        let Some(chain) = selection(&jump)?.map(|index| chains[index].clone())
                        else {
                            status.set_text("请选择目标子链。")?;
                            continue;
                        };
                        PreroutingAction::Jump { chain }
                    }
                    action @ (5 | 6) => {
                        let Some(target) = read_target(&primary)? else {
                            status
                                .set_text("请输入 IPv4 或 IPv6 地址。端口可留空或填 1–65535。")?;
                            continue;
                        };
                        if action == 6 {
                            PreroutingAction::SourceNat(target)
                        } else {
                            let source = match selection(&combined)? {
                                Some(1) => {
                                    let Some(source) = read_target(&secondary)? else {
                                        status.set_text("请输入有效的源转换地址与可选端口。")?;
                                        continue;
                                    };
                                    Some(NatSource::Fixed(source))
                                }
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
                };
                return Ok(Some(result));
            }
            _ => {}
        }
    }
    Ok(None)
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
    address: &str,
    port: &str,
    current: Option<NatTarget>,
) -> liteos_ui::Result<(Widget, Widget)> {
    let (_, address) = field(
        parent,
        address,
        &current.map_or_else(String::new, |value| value.address.to_string()),
    )?;
    let (_, port) = field(
        parent,
        port,
        &current
            .and_then(|value| value.port)
            .map_or_else(String::new, |value| value.to_string()),
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
