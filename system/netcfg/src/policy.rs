//! 配置来源：bootfs 只读默认策略（interfaces(5) 子集）与私有数据目录中的用户设置。

use alloc::{string::String, vec::Vec};
use core::net::Ipv4Addr;

use netcfg_protocol::{
    Decoder, Encoder, InterfaceConfig, Invalid, Method, connection_name, dns_suffix,
};

/// 用户设置在私有数据目录中的文件名。
pub(crate) const SETTINGS_FILE: &[u8] = b"interfaces";
/// 版本 2 至 5 分别添加连接名称、接口 DNS 后缀、全局搜索列表和主后缀设置。
const SETTINGS_VERSION: u8 = 5;

/// 一条配置：接口名或通配符 `*`，以及该接口的设置。
pub(crate) type Entry = (Vec<u8>, InterfaceConfig);

#[derive(Clone)]
pub(crate) struct Settings {
    pub(crate) interfaces: Vec<Entry>,
    pub(crate) names: Vec<(Vec<u8>, String)>,
    pub(crate) search_list: Vec<String>,
    pub(crate) primary_dns_suffix: String,
    pub(crate) append_parent_suffixes: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interfaces: Vec::new(),
            names: Vec::new(),
            search_list: Vec::new(),
            primary_dns_suffix: String::new(),
            append_parent_suffixes: true,
        }
    }
}

/// 解析默认策略。只识别 `iface <名称|*> inet <dhcp|static|disabled>` 段及其下的 `address`、`netmask`、
/// `gateway`、`dns-nameservers`；其余行忽略（与 ifupdown 对未知选项的处理一致）。
pub(crate) fn parse_defaults(text: &[u8]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.split(|byte| *byte == b'\n') {
        let mut words = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|word| !word.is_empty());
        let Some(keyword) = words.next() else {
            continue;
        };
        if keyword.starts_with(b"#") {
            continue;
        }
        if keyword == b"iface" {
            let (Some(name), Some(b"inet"), Some(method)) =
                (words.next(), words.next(), words.next())
            else {
                continue;
            };
            let method = match method {
                b"dhcp" => Method::Dhcp,
                b"static" => Method::Static,
                b"manual" | b"disabled" => Method::Disabled,
                _ => continue,
            };
            let mut config = InterfaceConfig::dhcp();
            config.method = method;
            entries.push((name.to_vec(), config));
            continue;
        }
        let Some((_, config)) = entries.last_mut() else {
            continue;
        };
        let address = |word: Option<&[u8]>| {
            word.and_then(|word| core::str::from_utf8(word).ok())
                .and_then(|word| word.parse::<Ipv4Addr>().ok())
        };
        match keyword {
            b"address" => {
                if let Some(value) = address(words.next()) {
                    config.address = value;
                }
            }
            b"netmask" => {
                if let Some(value) = address(words.next()) {
                    config.prefix_length = u32::from(value).leading_ones() as u8;
                }
            }
            b"gateway" => {
                if let Some(value) = address(words.next()) {
                    config.gateway = value;
                }
            }
            b"dns-nameservers" => {
                config.dns = words
                    .filter_map(|word| address(Some(word)))
                    .take(netcfg_protocol::DNS_MAX)
                    .collect();
            }
            _ => {}
        }
    }
    entries
}

/// 接口 `name` 的设置：用户设置优先，其次默认策略中的同名段，再次通配段，最后 DHCP。
pub(crate) fn resolve(saved: &[Entry], defaults: &[Entry], name: &[u8]) -> InterfaceConfig {
    saved
        .iter()
        .chain(defaults)
        .find(|(entry, _)| entry.as_slice() == name)
        .or_else(|| defaults.iter().find(|(entry, _)| entry.as_slice() == b"*"))
        .map_or_else(InterfaceConfig::dhcp, |(_, config)| config.clone())
}

/// 校验用户提交的设置：静态地址必须有地址与 1..=32 的前缀。
pub(crate) fn valid(config: &InterfaceConfig) -> bool {
    config.method != Method::Static
        || (!config.address.is_unspecified() && (1..=32).contains(&config.prefix_length))
}

/// 编码用户设置文件。
pub(crate) fn encode_settings(settings: &Settings) -> Vec<u8> {
    let mut encoder = Encoder::default();
    encoder
        .u8(SETTINGS_VERSION)
        .u32(settings.interfaces.len() as u32);
    for (name, config) in &settings.interfaces {
        encoder.bytes(name).config(config);
    }
    encoder.u32(settings.names.len() as u32);
    for (interface, name) in &settings.names {
        encoder.bytes(interface).bytes(name.as_bytes());
    }
    encoder
        .search_list(&settings.search_list)
        .bytes(settings.primary_dns_suffix.as_bytes())
        .u8(u8::from(settings.append_parent_suffixes));
    encoder.0
}

/// 解码用户设置文件；版本不符或格式错误时视为没有用户设置。
pub(crate) fn decode_settings(bytes: &[u8]) -> Result<Settings, Invalid> {
    let mut decoder = Decoder::new(bytes);
    let version = decoder.u8()?;
    if !(1..=SETTINGS_VERSION).contains(&version) {
        return Err(Invalid);
    }
    let count = decoder.u32()? as usize;
    if count > bytes.len() {
        return Err(Invalid);
    }
    let mut interfaces = Vec::with_capacity(count);
    for _ in 0..count {
        let name = decoder.bytes()?.to_vec();
        let config = decoder.config(version >= 3)?;
        interfaces.push((name, config));
    }
    let mut names = Vec::new();
    if version >= 2 {
        let count = decoder.u32()? as usize;
        for _ in 0..count {
            let interface = decoder.bytes()?;
            if interface.is_empty() || interface.len() >= netcfg_protocol::NAME_MAX {
                return Err(Invalid);
            }
            names.push((
                interface.to_vec(),
                connection_name(decoder.bytes()?)?.into(),
            ));
        }
    }
    let search_list = if version >= 4 {
        decoder.search_list()?
    } else {
        Vec::new()
    };
    let mut settings = Settings {
        interfaces,
        names,
        search_list,
        ..Settings::default()
    };
    if version >= 5 {
        settings.primary_dns_suffix = dns_suffix(decoder.bytes()?)?.into();
        settings.append_parent_suffixes = match decoder.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Invalid),
        };
    }
    decoder.finish()?;
    Ok(settings)
}
