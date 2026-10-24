//! 协议描述模型，以及从 XML 元素树到模型的转换与校验。

use crate::xml::{self, Element};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    Int,
    Uint,
    Fixed,
    String,
    Object,
    NewId,
    Array,
    Fd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    pub name: String,
    pub kind: ArgKind,
    pub allow_null: bool,
    /// `object` 与带类型 `new_id` 的目标接口名；`wl_display.error` 的 object 没有接口，不做目标校验。
    pub interface: Option<String>,
    /// `uint`/`int` 引用的枚举，`interface.enum` 或同接口内的 `enum`。
    pub enumeration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub name: String,
    pub since: u32,
    pub destructor: bool,
    pub summary: String,
    pub args: Vec<Arg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub value: u32,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enumeration {
    pub name: String,
    pub bitfield: bool,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub version: u32,
    pub requests: Vec<Message>,
    pub events: Vec<Message>,
    pub enums: Vec<Enumeration>,
}

fn required<'a>(element: &'a Element, name: &str) -> Result<&'a str, String> {
    element
        .attribute(name)
        .ok_or_else(|| format!("<{}> is missing `{name}`", element.name))
}

fn number(text: &str) -> Result<u32, String> {
    let parsed = match text.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|_| format!("invalid number `{text}`"))
}

fn arg(element: &Element) -> Result<Arg, String> {
    let name = required(element, "name")?.to_owned();
    let kind = match required(element, "type")? {
        "int" => ArgKind::Int,
        "uint" => ArgKind::Uint,
        "fixed" => ArgKind::Fixed,
        "string" => ArgKind::String,
        "object" => ArgKind::Object,
        "new_id" => ArgKind::NewId,
        "array" => ArgKind::Array,
        "fd" => ArgKind::Fd,
        other => return Err(format!("arg `{name}`: unknown type `{other}`")),
    };
    let interface = element.attribute("interface").map(str::to_owned);
    Ok(Arg {
        name,
        kind,
        allow_null: element.attribute("allow-null") == Some("true"),
        interface,
        enumeration: element.attribute("enum").map(str::to_owned),
    })
}

fn message(element: &Element) -> Result<Message, String> {
    let name = required(element, "name")?.to_owned();
    let args = element
        .children_named("arg")
        .map(arg)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{name}: {error}"))?;
    if args.iter().filter(|arg| arg.kind == ArgKind::NewId).count() > 1 {
        return Err(format!("{name}: more than one new_id"));
    }
    Ok(Message {
        since: element
            .attribute("since")
            .map(number)
            .transpose()?
            .unwrap_or(1),
        destructor: element.attribute("type") == Some("destructor"),
        summary: description_summary(element),
        name,
        args,
    })
}

fn description_summary(element: &Element) -> String {
    element
        .children_named("description")
        .next()
        .and_then(|description| description.attribute("summary"))
        .unwrap_or_default()
        .to_owned()
}

fn enumeration(element: &Element) -> Result<Enumeration, String> {
    let name = required(element, "name")?.to_owned();
    let mut entries: Vec<Entry> = Vec::new();
    for entry in element.children_named("entry") {
        let entry_name = required(entry, "name")?.to_owned();
        let value = number(required(entry, "value")?)
            .map_err(|error| format!("{name}.{entry_name}: {error}"))?;
        if let Some(existing) = entries
            .iter()
            .find(|existing| existing.value == value || existing.name == entry_name)
        {
            return Err(format!(
                "enum {name}: `{entry_name}` duplicates `{}`",
                existing.name
            ));
        }
        entries.push(Entry {
            name: entry_name,
            value,
            summary: entry.attribute("summary").unwrap_or_default().to_owned(),
        });
    }
    Ok(Enumeration {
        name,
        bitfield: element.attribute("bitfield") == Some("true"),
        entries,
    })
}

fn interface(element: &Element) -> Result<Interface, String> {
    let name = required(element, "name")?.to_owned();
    let wrap = |error: String| format!("{name}: {error}");
    Ok(Interface {
        version: number(required(element, "version").map_err(wrap)?).map_err(wrap)?,
        requests: element
            .children_named("request")
            .map(message)
            .collect::<Result<_, _>>()
            .map_err(wrap)?,
        events: element
            .children_named("event")
            .map(message)
            .collect::<Result<_, _>>()
            .map_err(wrap)?,
        enums: element
            .children_named("enum")
            .map(enumeration)
            .collect::<Result<_, _>>()
            .map_err(wrap)?,
        name,
    })
}

/// 解析一份协议 XML，返回其中按出现顺序排列的接口。
///
/// # Errors
///
/// XML 语法错误、缺少必需属性、重复的枚举项或无法识别的参数类型。
pub fn parse(text: &str) -> Result<Vec<Interface>, String> {
    let root = xml::parse(text)?;
    if root.name != "protocol" {
        return Err(format!(
            "root element is <{}>, expected <protocol>",
            root.name
        ));
    }
    root.children_named("interface").map(interface).collect()
}
