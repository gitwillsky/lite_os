//! 四段式 IP 地址输入框（comctl32 SysIPAddress32）：凹陷白框内四个无边框字段与圆点；
//! 输入“.”或满三位时焦点移到下一段，每段 0–255，四段都空表示未填写。
use crate::child;
use liteos_ui::{Event, EventKind, Layout, Widget, WidgetKind as Kind, state};
use std::net::Ipv4Addr;

pub struct IpAddress {
    frame: Widget,
    fields: [Widget; 4],
    _dots: Vec<Widget>,
}

fn fixed(width: i32) -> Layout {
    Layout {
        width,
        gap: 0,
        ..Default::default()
    }
}

impl IpAddress {
    /// 在 `parent` 中创建；SysIPAddress32 的 83×11 DLU 约为 125×17 逻辑像素。
    pub fn new(parent: &Widget) -> liteos_ui::Result<Self> {
        let frame = child(
            parent,
            Kind::Row,
            "",
            Layout {
                width: 125,
                height: 20,
                padding: 1,
                gap: 0,
                ..Default::default()
            },
        )?;
        frame.set_state(state::CLIENT_EDGE)?;
        frame.set_background(Some(0xFF_FFFF), None)?;
        let mut fields = Vec::new();
        let mut dots = Vec::new();
        for index in 0..4 {
            if index > 0 {
                dots.push(child(&frame, Kind::Label, ".", fixed(5))?);
            }
            let field = child(&frame, Kind::Edit, "", fixed(24))?;
            field.set_state(state::BORDERLESS)?;
            fields.push(field);
        }
        Ok(Self {
            frame,
            fields: fields.try_into().ok().expect("four octets"),
            _dots: dots,
        })
    }

    /// 显示地址；None 清空四段。
    pub fn set(&self, address: Option<Ipv4Addr>) -> liteos_ui::Result<()> {
        let octets = address.map(|address| address.octets());
        for (index, field) in self.fields.iter().enumerate() {
            field.set_text(&octets.map_or_else(String::new, |octets| octets[index].to_string()))?;
        }
        Ok(())
    }

    /// 启用或禁用整个控件（单选“自动获得”时变灰）。
    pub fn set_enabled(&self, enabled: bool) -> liteos_ui::Result<()> {
        let flag = if enabled { 0 } else { state::DISABLED };
        for field in &self.fields {
            field.set_state(state::BORDERLESS | flag)?;
        }
        self.frame.set_state(state::CLIENT_EDGE | flag)
    }

    /// 读取地址：四段都空为 Ok(None)；有段空、非数字或超过 255 为 Err(出错段)。
    pub fn value(&self) -> Result<Option<Ipv4Addr>, Widget> {
        let texts: Vec<String> = self
            .fields
            .iter()
            .map(|field| field.text().unwrap_or_default().trim().to_owned())
            .collect();
        if texts.iter().all(String::is_empty) {
            return Ok(None);
        }
        let mut octets = [0u8; 4];
        for (index, text) in texts.iter().enumerate() {
            octets[index] = text.parse().map_err(|_| self.fields[index].clone())?;
        }
        Ok(Some(Ipv4Addr::from(octets)))
    }

    /// 事件是否属于本控件。
    pub fn owns(&self, event: &Event) -> bool {
        self.fields.iter().any(|field| field.id() == event.node)
    }

    /// 编辑中的自动跳段：输入“.”时去掉它并跳到下一段，满三位数字时跳到下一段。
    pub fn event(&self, event: &Event) -> liteos_ui::Result<()> {
        if !matches!(event.kind, EventKind::Changed { .. }) {
            return Ok(());
        }
        let Some(index) = self
            .fields
            .iter()
            .position(|field| field.id() == event.node)
        else {
            return Ok(());
        };
        let text = self.fields[index].text()?;
        let digits: String = text.chars().filter(char::is_ascii_digit).take(3).collect();
        let advance = text.contains('.') || digits.len() == 3;
        if digits != text {
            self.fields[index].set_text(&digits)?;
        }
        if advance && let Some(next) = self.fields.get(index + 1) {
            next.focus()?;
            next.edit(liteos_ui::EditCommand::SelectAll)?;
        }
        Ok(())
    }
}
