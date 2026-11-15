//! 防火墙规则属性。应用后绑定到确切的提交代际，在同一窗口继续编辑。

use super::catalog::{Catalog, Entry, Key, Scope};
use crate::{
    child,
    item_list::{Glyph, Item, Tool, note, set_note, toolbar},
    item_properties::{Footer, Request, field},
    routes::{InputError, RuleFields, RuleSources, rule_tab},
    workspace::Workspace,
};
use liteos_ui::{Event, Layout, Widget, WidgetKind, state};
use network_config::{Revision, configuration::PreroutingRule};

pub(super) struct Property {
    pub(super) frame: Widget,
    pub(super) revision: Revision,
    pub(super) original: Option<(Key, PreroutingRule)>,
    fields: RuleFields,
    bytes: Widget,
    packets: Widget,
    statistics: Widget,
    actions: Widget,
    remove: Tool,
    footer: Footer,
    message: Widget,
    _layout: Vec<Widget>,
}

impl Property {
    pub(super) fn new(
        workspace: &Workspace,
        catalog: &Catalog,
        scope: Scope,
        entry: Option<&Entry>,
    ) -> liteos_ui::Result<Self> {
        let title = if let Some(entry) = entry {
            format!("防火墙规则 > {}", title_value(&entry.rule))
        } else {
            "新建防火墙规则".into()
        };
        let frame = workspace.item_window(&title, (550, 369))?;
        let grow = Layout {
            grow: 1,
            ..Layout::default()
        };
        let root = child(
            &frame,
            WidgetKind::Column,
            "",
            Layout {
                padding: 4,
                gap: 4,
                ..grow
            },
        )?;
        let body = child(&root, WidgetKind::Row, "", Layout { gap: 6, ..grow })?;
        let tabs = child(&body, WidgetKind::Tabs, "", grow)?;
        let chain = entry.and_then(|entry| entry.key.chain.as_ref());
        let targets: Vec<_> = catalog
            .chains
            .iter()
            .filter(|target| Some(*target) != chain)
            .collect();
        let sets: Vec<_> = catalog.sets.iter().collect();
        let fields = RuleFields::new(
            &tabs,
            RuleSources {
                interfaces: &catalog.interfaces,
                chains: &targets,
                sets: &sets,
                chain: chain.map_or("prerouting", |name| name.as_str()),
            },
            entry.map(|entry| &entry.rule),
            Some((scope.family, scope.kind)),
        )?;
        let statistics = rule_tab(&tabs, "统计")?;
        let mut layout = Vec::new();
        let bytes = field(
            &statistics,
            &mut layout,
            "字节数",
            WidgetKind::Edit,
            "",
            true,
        )?;
        let packets = field(
            &statistics,
            &mut layout,
            "报文数",
            WidgetKind::Edit,
            "",
            true,
        )?;
        let actions = child(
            &body,
            WidgetKind::Column,
            "",
            Layout {
                width: 94,
                ..Layout::default()
            },
        )?;
        let [remove] = toolbar(&actions, [Item::Command(Glyph::Remove, "删除")])?;
        let message = note(&root)?;
        let footer = Footer::new(&root)?;
        layout.extend([root, body, tabs]);
        let property = Self {
            frame,
            revision: catalog.revision,
            original: entry.map(|entry| (entry.key.clone(), entry.rule.clone())),
            fields,
            bytes,
            packets,
            statistics,
            actions,
            remove,
            footer,
            message,
            _layout: layout,
        };
        property.ready(true)?;
        Ok(property)
    }

    pub(super) fn request(&self, event: &Event) -> liteos_ui::Result<Option<Request>> {
        self.fields.handle(event)?;
        Ok(self.footer.request(&self.frame, event).or_else(|| {
            (self.remove.activated(event) && self.original.is_some()).then_some(Request::Remove)
        }))
    }

    pub(super) fn value(&self) -> liteos_ui::Result<Option<PreroutingRule>> {
        match self
            .fields
            .value(self.original.as_ref().map(|(_, rule)| rule))
        {
            Ok(rule) => Ok(Some(rule)),
            Err(InputError::Control(error)) => Err(error),
            Err(InputError::Invalid(message)) => {
                self.note(message)?;
                Ok(None)
            }
        }
    }

    pub(super) fn note(&self, text: &str) -> liteos_ui::Result<()> {
        set_note(&self.message, text)
    }

    pub(super) fn ready(&self, enabled: bool) -> liteos_ui::Result<()> {
        let visible = if self.original.is_some() {
            0
        } else {
            state::HIDDEN
        };
        self.statistics.set_state(visible)?;
        self.actions.set_state(visible)?;
        self.remove.set_enabled(enabled)?;
        self.footer.set_enabled(enabled, enabled)
    }

    pub(super) fn close_only(&self) -> liteos_ui::Result<()> {
        self.remove.set_enabled(false)?;
        self.footer.set_enabled(true, false)
    }

    pub(super) fn counts(&self, counts: Option<(u64, u64)>) -> liteos_ui::Result<()> {
        let (bytes, packets) = counts.map_or_else(
            || (String::new(), String::new()),
            |(bytes, packets)| (format!("{bytes} B"), packets.to_string()),
        );
        self.bytes.set_text(&bytes)?;
        self.packets.set_text(&packets)
    }

    pub(super) fn bind(
        &mut self,
        revision: Revision,
        key: Key,
        rule: PreroutingRule,
    ) -> liteos_ui::Result<()> {
        self.frame
            .set_text(&format!("防火墙规则 > {}", title_value(&rule)))?;
        self.revision = revision;
        self.original = Some((key, rule));
        self.ready(true)
    }
}

fn title_value(rule: &PreroutingRule) -> String {
    if let Some(address) = rule.destination {
        return address.to_string();
    }
    if let Some(source) = rule.source {
        return source.network.to_string();
    }
    network_config::firewall::configured_action(&rule.action)
}
