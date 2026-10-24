//! 把协议模型生成为 Rust 源码：每个接口一个模块，含枚举、`Request`、`Event` 及其 wire 编解码。
//!
//! 生成代码依赖宿主 crate 提供的 `crate::wire`：`ObjectId`、`Fixed`、`DynamicNewId`、`Reader`、`Writer`、
//! `DecodeError`、`EncodeError`。服务端与客户端使用同一组类型，只是编码与解码的方向相反。

use std::fmt::Write;

use crate::protocol::{Arg, ArgKind, Enumeration, Interface, Message};

const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn",
];

fn camel(name: &str) -> String {
    let mut out = String::new();
    for part in name.split('_').filter(|part| !part.is_empty()) {
        let mut chars = part.chars();
        out.extend(chars.next().map(|first| first.to_ascii_uppercase()));
        out.push_str(chars.as_str());
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn field(name: &str) -> String {
    if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_owned()
    }
}

fn doc(out: &mut String, indent: &str, summary: &str) {
    if !summary.is_empty() {
        let _ = writeln!(out, "{indent}/// {}", summary.replace('\n', " "));
    }
}

/// 把 `enum` 属性解析成生成代码里的类型路径；位域与未知枚举返回错误或 `None`。
fn enum_type(
    interfaces: &[Interface],
    owner: &str,
    reference: &str,
) -> Result<Option<String>, String> {
    let (interface_name, enum_name) = reference.split_once('.').unwrap_or((owner, reference));
    let target = interfaces
        .iter()
        .find(|interface| interface.name == interface_name)
        .and_then(|interface| {
            interface
                .enums
                .iter()
                .find(|candidate| candidate.name == enum_name)
        })
        .ok_or_else(|| format!("{owner}: enum `{reference}` is not defined"))?;
    if target.bitfield {
        return Ok(None);
    }
    Ok(Some(if interface_name == owner {
        camel(enum_name)
    } else {
        format!("super::{interface_name}::{}", camel(enum_name))
    }))
}

fn rust_type(interfaces: &[Interface], owner: &str, arg: &Arg) -> Result<String, String> {
    Ok(match arg.kind {
        ArgKind::Int => "i32".into(),
        ArgKind::Uint => match &arg.enumeration {
            Some(reference) => {
                enum_type(interfaces, owner, reference)?.unwrap_or_else(|| "u32".into())
            }
            None => "u32".into(),
        },
        ArgKind::Fixed => "Fixed".into(),
        ArgKind::String if arg.allow_null => "Option<String>".into(),
        ArgKind::String => "String".into(),
        ArgKind::Object if arg.allow_null => "Option<ObjectId>".into(),
        ArgKind::Object => "ObjectId".into(),
        ArgKind::NewId if arg.interface.is_some() => "ObjectId".into(),
        ArgKind::NewId => "DynamicNewId".into(),
        ArgKind::Array => "Vec<u8>".into(),
        ArgKind::Fd => "std::os::fd::OwnedFd".into(),
    })
}

fn decode_expression(interfaces: &[Interface], owner: &str, arg: &Arg) -> Result<String, String> {
    Ok(match arg.kind {
        ArgKind::Int => "reader.int()?".into(),
        ArgKind::Uint => match arg
            .enumeration
            .as_deref()
            .map(|r| enum_type(interfaces, owner, r))
            .transpose()?
            .flatten()
        {
            Some(ty) => format!("{ty}::from_u32(reader.uint()?).ok_or(DecodeError::InvalidEnum)?"),
            None => "reader.uint()?".into(),
        },
        ArgKind::Fixed => "reader.fixed()?".into(),
        ArgKind::String if arg.allow_null => "reader.string()?".into(),
        ArgKind::String => "reader.string()?.ok_or(DecodeError::UnexpectedNull)?".into(),
        ArgKind::Object if arg.allow_null => "reader.object()?".into(),
        ArgKind::Object => "reader.object()?.ok_or(DecodeError::UnexpectedNull)?".into(),
        ArgKind::NewId if arg.interface.is_some() => "reader.new_id()?".into(),
        ArgKind::NewId => "reader.dynamic_new_id()?".into(),
        ArgKind::Array => "reader.array()?".into(),
        ArgKind::Fd => "reader.fd()?".into(),
    })
}

fn encode_statement(interfaces: &[Interface], owner: &str, arg: &Arg) -> Result<String, String> {
    let name = field(&arg.name);
    Ok(match arg.kind {
        ArgKind::Int => format!("writer.int(*{name});"),
        ArgKind::Uint => {
            let is_enum = arg
                .enumeration
                .as_deref()
                .map(|r| enum_type(interfaces, owner, r))
                .transpose()?
                .flatten()
                .is_some();
            if is_enum {
                format!("writer.uint(*{name} as u32);")
            } else {
                format!("writer.uint(*{name});")
            }
        }
        ArgKind::Fixed => format!("writer.fixed(*{name});"),
        ArgKind::String if arg.allow_null => format!("writer.string({name}.as_deref());"),
        ArgKind::String => format!("writer.string(Some({name}));"),
        ArgKind::Object if arg.allow_null => format!("writer.object(*{name});"),
        ArgKind::Object => format!("writer.object(Some(*{name}));"),
        ArgKind::NewId if arg.interface.is_some() => format!("writer.object(Some(*{name}));"),
        ArgKind::NewId => format!("writer.dynamic_new_id({name});"),
        ArgKind::Array => format!("writer.array({name});"),
        ArgKind::Fd => format!("writer.fd(std::os::fd::AsRawFd::as_raw_fd({name}));"),
    })
}

fn write_enum(out: &mut String, enumeration: &Enumeration) {
    if enumeration.bitfield {
        let _ = writeln!(out, "    pub mod {} {{", enumeration.name);
        for entry in &enumeration.entries {
            doc(out, "        ", &entry.summary);
            let _ = writeln!(
                out,
                "        pub const {}: u32 = {};",
                camel(&entry.name).to_uppercase(),
                entry.value
            );
        }
        out.push_str("    }\n");
        return;
    }
    let name = camel(&enumeration.name);
    let _ = writeln!(
        out,
        "    #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n    #[repr(u32)]\n    pub enum {name} {{"
    );
    for entry in &enumeration.entries {
        doc(out, "        ", &entry.summary);
        let _ = writeln!(out, "        {} = {},", camel(&entry.name), entry.value);
    }
    let _ = writeln!(
        out,
        "    }}\n    impl {name} {{\n        pub fn from_u32(value: u32) -> Option<Self> {{\n            match value {{"
    );
    for entry in &enumeration.entries {
        let _ = writeln!(
            out,
            "                {} => Some(Self::{}),",
            entry.value,
            camel(&entry.name)
        );
    }
    out.push_str("                _ => None,\n            }\n        }\n    }\n");
}

fn write_messages(
    out: &mut String,
    interfaces: &[Interface],
    owner: &str,
    type_name: &str,
    messages: &[Message],
) -> Result<(), String> {
    let _ = writeln!(out, "    #[derive(Debug)]\n    pub enum {type_name} {{");
    for message in messages {
        doc(out, "        ", &message.summary);
        if message.args.is_empty() {
            let _ = writeln!(out, "        {},", camel(&message.name));
            continue;
        }
        let _ = writeln!(out, "        {} {{", camel(&message.name));
        for arg in &message.args {
            let _ = writeln!(
                out,
                "            {}: {},",
                field(&arg.name),
                rust_type(interfaces, owner, arg)?
            );
        }
        out.push_str("        },\n");
    }
    let _ = writeln!(out, "    }}\n    impl {type_name} {{");

    // opcode / since / destructor
    for (function, ty, value) in [
        ("opcode", "u16", None::<fn(usize, &Message) -> String>),
        (
            "since",
            "u32",
            Some(|_, message: &Message| message.since.to_string()),
        ),
    ] {
        let _ = writeln!(
            out,
            "        pub fn {function}(&self) -> {ty} {{\n            match *self {{"
        );
        for (index, message) in messages.iter().enumerate() {
            let result = value.map_or_else(|| index.to_string(), |f| f(index, message));
            let _ = writeln!(
                out,
                "                Self::{} {{ .. }} => {result},",
                camel(&message.name)
            );
        }
        out.push_str("            }\n        }\n");
    }
    let _ = writeln!(
        out,
        "        /// 该消息是否销毁其发送对象（`type=\"destructor\"`）。\n        pub fn is_destructor(&self) -> bool {{\n            match *self {{"
    );
    for message in messages {
        let _ = writeln!(
            out,
            "                Self::{} {{ .. }} => {},",
            camel(&message.name),
            message.destructor
        );
    }
    out.push_str("            }\n        }\n");

    // encode
    if messages.is_empty() {
        out.push_str("        pub fn encode(&self, _object: ObjectId, _writer: &mut Writer) -> Result<(), EncodeError> {\n            match *self {}\n        }\n");
    } else {
        let _ = writeln!(
            out,
            "        pub fn encode(&self, object: ObjectId, writer: &mut Writer) -> Result<(), EncodeError> {{\n            match self {{"
        );
        for (index, message) in messages.iter().enumerate() {
            let names: Vec<String> = message.args.iter().map(|arg| field(&arg.name)).collect();
            let pattern = if names.is_empty() {
                String::new()
            } else {
                format!(" {{ {} }}", names.join(", "))
            };
            let _ = writeln!(
                out,
                "                Self::{}{pattern} => {{\n                    writer.begin(object, {index});",
                camel(&message.name)
            );
            for arg in &message.args {
                let _ = writeln!(
                    out,
                    "                    {}",
                    encode_statement(interfaces, owner, arg)?
                );
            }
            out.push_str("                    writer.end()\n                }\n");
        }
        out.push_str("            }\n        }\n");
    }

    // decode
    if messages.is_empty() {
        out.push_str("        pub fn decode(opcode: u16, _reader: &mut Reader<'_>) -> Result<Self, DecodeError> {\n            Err(DecodeError::UnknownOpcode(opcode))\n        }\n    }\n");
        return Ok(());
    }
    let _ = writeln!(
        out,
        "        pub fn decode(opcode: u16, reader: &mut Reader<'_>) -> Result<Self, DecodeError> {{\n            let message = match opcode {{"
    );
    for (index, message) in messages.iter().enumerate() {
        if message.args.is_empty() {
            let _ = writeln!(
                out,
                "                {index} => Self::{},",
                camel(&message.name)
            );
            continue;
        }
        let _ = writeln!(
            out,
            "                {index} => Self::{} {{",
            camel(&message.name)
        );
        for arg in &message.args {
            let _ = writeln!(
                out,
                "                    {}: {},",
                field(&arg.name),
                decode_expression(interfaces, owner, arg)?
            );
        }
        out.push_str("                },\n");
    }
    out.push_str("                _ => return Err(DecodeError::UnknownOpcode(opcode)),\n            };\n            reader.finish()?;\n            Ok(message)\n        }\n    }\n");
    Ok(())
}

fn validate(interfaces: &[Interface]) -> Result<(), String> {
    for interface in interfaces {
        for message in interface.requests.iter().chain(&interface.events) {
            for arg in &message.args {
                if let Some(target) = &arg.interface
                    && !interfaces.iter().any(|candidate| &candidate.name == target)
                {
                    return Err(format!(
                        "{}.{}: unknown interface `{target}`",
                        interface.name, message.name
                    ));
                }
            }
        }
    }
    let mut names: Vec<&str> = interfaces
        .iter()
        .map(|interface| interface.name.as_str())
        .collect();
    names.sort_unstable();
    if let Some(pair) = names.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(format!("interface `{}` is defined twice", pair[0]));
    }
    Ok(())
}

/// 为 `interfaces` 生成完整的 Rust 源码。
///
/// # Errors
///
/// 对象参数引用未知接口、枚举引用无法解析或接口重复定义。
pub fn generate(interfaces: &[Interface]) -> Result<String, String> {
    validate(interfaces)?;
    let mut out = String::from("// @generated by wayland-scanner; do not edit.\n\n");
    for interface in interfaces {
        let _ = writeln!(
            out,
            "pub mod {} {{\n    // 每个接口导入同一组类型，没有用到某一类参数的接口不应触发 unused_imports。\n    #[allow(unused_imports)]\n    use crate::wire::{{DecodeError, DynamicNewId, EncodeError, Fixed, ObjectId, Reader, Writer}};\n",
            interface.name
        );
        let _ = writeln!(
            out,
            "    pub const NAME: &str = \"{}\";\n    pub const VERSION: u32 = {};\n",
            interface.name, interface.version
        );
        for enumeration in &interface.enums {
            write_enum(&mut out, enumeration);
        }
        write_messages(
            &mut out,
            interfaces,
            &interface.name,
            "Request",
            &interface.requests,
        )?;
        write_messages(
            &mut out,
            interfaces,
            &interface.name,
            "Event",
            &interface.events,
        )?;
        out.push_str("}\n\n");
    }

    out.push_str("/// 协议中出现的全部接口。\n#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\npub enum Interface {\n");
    for interface in interfaces {
        let _ = writeln!(out, "    {},", camel(&interface.name));
    }
    out.push_str("}\n\nimpl Interface {\n    pub const ALL: &'static [Interface] = &[\n");
    for interface in interfaces {
        let _ = writeln!(out, "        Interface::{},", camel(&interface.name));
    }
    out.push_str("    ];\n\n    pub fn name(self) -> &'static str {\n        match self {\n");
    for interface in interfaces {
        let _ = writeln!(
            out,
            "            Interface::{} => {}::NAME,",
            camel(&interface.name),
            interface.name
        );
    }
    out.push_str("        }\n    }\n\n    /// 本实现支持的最高版本。\n    pub fn version(self) -> u32 {\n        match self {\n");
    for interface in interfaces {
        let _ = writeln!(
            out,
            "            Interface::{} => {}::VERSION,",
            camel(&interface.name),
            interface.name
        );
    }
    out.push_str("        }\n    }\n\n    pub fn from_name(name: &str) -> Option<Interface> {\n        match name {\n");
    for interface in interfaces {
        let _ = writeln!(
            out,
            "            {}::NAME => Some(Interface::{}),",
            interface.name,
            camel(&interface.name)
        );
    }
    out.push_str("            _ => None,\n        }\n    }\n}\n");

    for (type_name, function) in [("Request", "decode_request"), ("Event", "decode_event")] {
        let _ = writeln!(out, "\n#[derive(Debug)]\npub enum Any{type_name} {{");
        for interface in interfaces {
            let _ = writeln!(
                out,
                "    {}({}::{type_name}),",
                camel(&interface.name),
                interface.name
            );
        }
        let _ = writeln!(
            out,
            "}}\n\n/// 按对象的接口解码一条 {type_name}。\npub fn {function}(interface: Interface, opcode: u16, reader: &mut crate::wire::Reader<'_>) -> Result<Any{type_name}, crate::wire::DecodeError> {{\n    Ok(match interface {{"
        );
        for interface in interfaces {
            let _ = writeln!(
                out,
                "        Interface::{0} => Any{type_name}::{0}({1}::{type_name}::decode(opcode, reader)?),",
                camel(&interface.name),
                interface.name
            );
        }
        out.push_str("    })\n}\n");
        let _ = writeln!(out, "\nimpl Any{type_name} {{");
        for (function, ty) in [("since", "u32"), ("is_destructor", "bool")] {
            let _ = writeln!(
                out,
                "    pub fn {function}(&self) -> {ty} {{\n        match self {{"
            );
            for interface in interfaces {
                let _ = writeln!(
                    out,
                    "            Any{type_name}::{}(message) => message.{function}(),",
                    camel(&interface.name)
                );
            }
            out.push_str("        }\n    }\n");
        }
        out.push_str("}\n");
    }
    Ok(out)
}
