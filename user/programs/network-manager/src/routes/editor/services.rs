//! 前缀委派与解析设置，所有修改由上层路由草稿一同提交。

use super::{button, form, grow, row};
use crate::child;
use liteos_ui::{Dialogs, EventKind, Layout, WidgetKind, Window};
use network_config::configuration::{ConfigurationName, RouterConfiguration};

mod delegation;
mod dns;

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    config: &mut RouterConfiguration,
    domain: Option<&ConfigurationName>,
) -> liteos_ui::Result<bool> {
    let (window, mut events, root) = form(dialogs, parent, "前缀委派与 DNS", 480, 141)?;
    let (interfaces, bridges) = domain.map_or((&config.interfaces, &config.bridges), |name| {
        let domain = &config.routing_domains[name];
        (&domain.interfaces, &domain.bridges)
    });
    let physical: Vec<_> = interfaces
        .keys()
        .filter(|&name| {
            !bridges
                .values()
                .any(|bridge| bridge.ports.contains_key(name))
        })
        .cloned()
        .collect();
    let mut names = physical.clone();
    for bridge in bridges.values() {
        for gateway in bridge.gateways.values() {
            names.push(gateway.name.clone());
        }
    }
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let pd = child(
        &bar,
        WidgetKind::Button,
        "IPv6 前缀委派…",
        Layout {
            width: 125,
            ..button()
        },
    )?;
    let dns = child(
        &bar,
        WidgetKind::Button,
        "DNS 设置…",
        Layout {
            width: 95,
            ..button()
        },
    )?;
    let _spacer = child(&bar, WidgetKind::Column, "", grow())?;
    let back = child(&bar, WidgetKind::Button, "返回", button())?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
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
            EventKind::Activate { .. } if event.node == dns.id() => {
                if let Some(next) = dns::edit(dialogs, &window, config, domain).await? {
                    if let Some(domain) = domain {
                        let target = &mut config
                            .routing_domains
                            .get_mut(domain)
                            .expect("selected domain")
                            .dns_forwarding;
                        changed |= *target != next.forwarding;
                        *target = next.forwarding;
                    } else {
                        changed |= config.dns != next;
                        config.dns = next;
                    }
                }
            }
            EventKind::Activate { .. } if event.node == pd.id() => {
                if let Some(next) =
                    delegation::edit(dialogs, &window, &physical, &names, config, domain).await?
                {
                    let target = match domain {
                        Some(name) => {
                            &mut config
                                .routing_domains
                                .get_mut(name)
                                .expect("selected domain")
                                .prefix_delegation
                        }
                        None => &mut config.prefix_delegation,
                    };
                    changed |= *target != next;
                    *target = next;
                }
            }
            _ => continue,
        }
        if changed {
            status.set_text("修改已加入路由草稿，返回后可一并审阅提交。")?;
        }
    }
    Ok(changed)
}
