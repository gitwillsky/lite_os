//! 按执行顺序编辑预路由入口、子链和地址集，修改留在同一份路由草稿。

use super::{
    button, checkbox, checked, choice, disabled, entries, field, form, form_buttons, grow, row,
    selection, set_choices,
};
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, Widget, WidgetKind, Window};
use network_config::configuration::{
    ConfigurationName, ExistingConnections, Firewall, PreroutingAction, valid_chain_name,
};

mod rule;
mod sets;

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &[ConfigurationName],
    firewall: &mut Firewall,
) -> liteos_ui::Result<bool> {
    let (window, mut events, root) = form(dialogs, parent, "防火墙与 NAT", 620, 247)?;
    let line = child(&root, WidgetKind::Row, "", row())?;
    let chain = choice(&line, std::iter::empty(), 205)?;
    let add = child(
        &line,
        WidgetKind::Button,
        "新建子链…",
        Layout {
            width: 105,
            ..button()
        },
    )?;
    let remove = child(
        &line,
        WidgetKind::Button,
        "删除子链",
        Layout {
            width: 97,
            ..button()
        },
    )?;
    let rules = child(&line, WidgetKind::Button, "规则…", button())?;
    let preserve = checkbox(
        &root,
        "让已有转发连接沿用原规则",
        firewall.existing_connections == ExistingConnections::Preserve,
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let sets = child(
        &bar,
        WidgetKind::Button,
        "目的地址集…",
        Layout {
            width: 114,
            ..button()
        },
    )?;
    let _spacer = child(&bar, WidgetKind::Column, "", grow())?;
    let back = child(&bar, WidgetKind::Button, "返回", button())?;
    let status = child(
        &root,
        WidgetKind::Label,
        "选择规则链后打开「规则…」。",
        Layout::default(),
    )?;
    select_chain(&chain, firewall, None)?;
    remove.set_state(disabled(true))?;
    let mut changed = false;

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == back.id() => break,
            EventKind::Select { .. } if event.node == chain.id() => {
                remove.set_state(disabled(current_chain(&chain, firewall)?.is_none()))?;
            }
            EventKind::Activate { .. } if event.node == add.id() => {
                if let Some(name) = new_chain(dialogs, &window, firewall).await? {
                    firewall.chains.insert(name.clone(), Vec::new());
                    select_chain(&chain, firewall, Some(&name))?;
                    remove.set_state(0)?;
                    changed = true;
                    status.set_text("已新建子链。通过跳转规则调用此链。")?;
                }
            }
            EventKind::Activate { .. } if event.node == remove.id() => {
                let Some(name) = current_chain(&chain, firewall)? else {
                    continue;
                };
                if firewall.rules().any(|rule| matches!(&rule.action, PreroutingAction::Jump { chain } if chain == &name)) {
                    status.set_text("仍有规则跳转到这条子链，请先修改对应规则。")?;
                    continue;
                }
                firewall.chains.remove(&name);
                select_chain(&chain, firewall, None)?;
                remove.set_state(disabled(true))?;
                changed = true;
                status.set_text("已从草稿删除子链及其中的规则。")?;
            }
            EventKind::Activate { .. } if event.node == sets.id() => {
                let Some(next) = sets::edit(dialogs, &window, &firewall.address_sets).await? else {
                    continue;
                };
                if let Some(name) = firewall
                    .rules()
                    .filter_map(|rule| rule.destination_set.as_ref())
                    .find(|name| !next.contains_key(*name))
                {
                    status.set_text(&format!(
                        "地址集 {} 仍被引用。请先修改对应规则。",
                        name.as_str()
                    ))?;
                    continue;
                }
                if next != firewall.address_sets {
                    firewall.address_sets = next;
                    changed = true;
                    status.set_text("地址集修改已加入草稿。")?;
                }
            }
            EventKind::Activate { .. } if event.node == rules.id() => {
                let name = current_chain(&chain, firewall)?;
                let targets: Vec<_> = firewall
                    .chains
                    .keys()
                    .filter(|&target| Some(target) != name.as_ref())
                    .collect();
                let sets: Vec<_> = firewall.address_sets.keys().collect();
                let current = name
                    .as_ref()
                    .map_or(&firewall.prerouting, |name| &firewall.chains[name]);
                let title = name
                    .as_ref()
                    .map_or("预路由入口", ConfigurationName::as_str);
                let Some(next) = entries::edit(
                    dialogs,
                    &window,
                    entries::ListLayout {
                        title,
                        columns: &[
                            Column {
                                id: 1,
                                title: "入接口",
                                width: 92,
                            },
                            Column {
                                id: 2,
                                title: "源网段",
                                width: 126,
                            },
                            Column {
                                id: 3,
                                title: "目的",
                                width: 128,
                            },
                            Column {
                                id: 4,
                                title: "动作",
                                width: 197,
                            },
                        ],
                        ordered: true,
                        allow_duplicates: true,
                    },
                    current.iter().cloned(),
                    rule::label,
                    async |window, items, index| {
                        rule::edit(
                            dialogs,
                            window,
                            interfaces,
                            &targets,
                            &sets,
                            index.map(|index| &items[index]),
                        )
                        .await
                    },
                )
                .await?
                else {
                    continue;
                };
                if next != *current {
                    let rules = if let Some(name) = &name {
                        firewall.chains.get_mut(name).expect("selected chain")
                    } else {
                        &mut firewall.prerouting
                    };
                    *rules = next;
                    changed = true;
                    status.set_text("规则修改已加入草稿。")?;
                }
            }
            _ => {}
        }
    }
    let mode = if checked(&preserve)? {
        ExistingConnections::Preserve
    } else {
        ExistingConnections::Reevaluate
    };
    changed |= firewall.existing_connections != mode;
    firewall.existing_connections = mode;
    Ok(changed)
}

fn current_chain(
    widget: &Widget,
    firewall: &Firewall,
) -> liteos_ui::Result<Option<ConfigurationName>> {
    Ok(selection(widget)?
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| firewall.chains.keys().nth(index))
        .cloned())
}

fn select_chain(
    widget: &Widget,
    firewall: &Firewall,
    active: Option<&ConfigurationName>,
) -> liteos_ui::Result<()> {
    set_choices(
        widget,
        std::iter::once("预路由入口").chain(firewall.chains.keys().map(ConfigurationName::as_str)),
    )?;
    let selected = active
        .and_then(|active| firewall.chains.keys().position(|name| name == active))
        .map_or(1, |index| index as u64 + 2);
    widget.select(Some(selected))
}

async fn new_chain(
    dialogs: &Dialogs,
    parent: &Window,
    firewall: &Firewall,
) -> liteos_ui::Result<Option<ConfigurationName>> {
    let (_window, mut events, root) = form(dialogs, parent, "新建防火墙子链", 454, 149)?;
    let (_, name) = field(&root, "子链名称", "")?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;
    name.focus()?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == save.id() => {
                let text = name.text()?.trim().to_owned();
                if !valid_chain_name(&text) {
                    status.set_text("请输入有效名称。prerouting 为系统保留。")?;
                    continue;
                }
                let Ok(value) = ConfigurationName::try_from(text) else {
                    status.set_text("子链名称无效。")?;
                    continue;
                };
                if firewall.chains.contains_key(&value) {
                    status.set_text("此子链名称已存在。")?;
                    continue;
                }
                return Ok(Some(value));
            }
            _ => {}
        }
    }
    Ok(None)
}
