//! IPv4 与 IPv6 防火墙工作区。每个窗口共用规则页和连接页的生命周期。

mod catalog;
mod rules;

use crate::{child, connections, routes::RuleKind, workspace::Workspace};
use liteos_ui::{Application, Background, Event, EventKind, Layout, Widget, WidgetKind, Window};
use network_config::configuration::connections::Family;
use std::sync::Arc;

pub(super) struct FirewallWindow {
    tabs: Widget,
    rules: [(Widget, rules::Page); 2],
    connections: (Widget, connections::Page),
}

impl FirewallWindow {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
        family: Family,
    ) -> liteos_ui::Result<Self> {
        let tabs = child(
            parent,
            WidgetKind::Tabs,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let pane = |title| {
            child(
                &tabs,
                WidgetKind::Column,
                title,
                Layout {
                    grow: 1,
                    padding: 3,
                    gap: 3,
                    ..Layout::default()
                },
            )
        };
        let rule_pane = |title, kind| {
            let parent = pane(title)?;
            let scope = catalog::Scope { family, kind };
            let view = rules::Page::new(app, &parent, background.clone(), scope)?;
            Ok((parent, view))
        };
        let rules = [
            rule_pane("过滤规则", RuleKind::Filter)?,
            rule_pane("NAT", RuleKind::Nat)?,
        ];
        let parent = pane("连接")?;
        let connections = connections::Page::new(app, &parent, background, family)?;
        Ok(Self {
            tabs,
            rules,
            connections: (parent, connections),
        })
    }

    pub(super) fn show(
        &mut self,
        app: &mut Application,
        window: &Window,
        open: bool,
    ) -> liteos_ui::Result<()> {
        let selected = self.tabs.selection()?;
        for (pane, rules) in &mut self.rules {
            rules.show(app, window, open && selected == Some(pane.id()))?;
        }
        let (pane, connections) = &mut self.connections;
        connections.selected_tab(app, window, open && selected == Some(pane.id()))
    }

    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        for (_, rules) in &mut self.rules {
            rules.select_domain(app, window, domain.clone())?;
        }
        self.connections.1.select_domain(app, window, domain)
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        workspace: &Workspace,
        event: &Event,
        open: bool,
    ) -> liteos_ui::Result<()> {
        if !open {
            return Ok(());
        }
        if event.node == self.tabs.id() && matches!(event.kind, EventKind::Select { .. }) {
            self.show(app, window, true)?;
        }
        for (_, rules) in &mut self.rules {
            rules.handle(app, window, event)?;
        }
        self.connections.1.handle(app, window, workspace, event)
    }
}
