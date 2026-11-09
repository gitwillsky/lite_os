//! 配置来源：bootfs 只读默认策略（interfaces(5) 子集）与私有数据目录中的用户设置。

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::net::Ipv4Addr;

use router_protocol::{Decoder, InterfaceConfig, Invalid, Method, connection_name, dns_suffix};

use router_protocol::configuration::{Configuration, DEFAULT_MTU, Ipv4Configuration};

/// 配置对象的稳定存储身份；格式迁移不创建第二份配置文件。
pub(crate) const SETTINGS_FILE: &[u8] = b"interfaces";
/// 版本 2 至 5 分别添加连接名称、接口 DNS 后缀、全局搜索列表和主后缀设置。
const SETTINGS_VERSION: u8 = 5;

/// 一条配置：接口名或通配符 `*`，以及该接口的设置。
pub(crate) type Entry = (Vec<u8>, InterfaceConfig);

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
                config.dns = words.filter_map(|word| address(Some(word))).collect();
            }
            _ => {}
        }
    }
    entries
}

/// 默认策略中的同名段优先于通配段；具名原生对象再按实际名字/MAC 覆盖选定字段。
pub(crate) fn resolve(
    saved: &Configuration,
    defaults: &[Entry],
    name: &[u8],
    mac: [u8; 6],
) -> Result<(InterfaceConfig, u32), Invalid> {
    let inherited = defaults
        .iter()
        .find(|(entry, _)| entry.as_slice() == name)
        .or_else(|| defaults.iter().find(|(entry, _)| entry.as_slice() == b"*"))
        .map_or_else(InterfaceConfig::dhcp, |(_, config)| config.clone());
    Ok(match saved.matching(name, mac)? {
        Some(profile) => (profile.project(inherited), profile.mtu),
        None => (inherited, DEFAULT_MTU),
    })
}

/// 当前只生成版本化原生 JSON；历史二进制没有写入路径。
pub(crate) fn encode_settings(settings: &Configuration) -> Result<Vec<u8>, Invalid> {
    settings.encode().map_err(|_| Invalid)
}

/// 读取唯一当前格式，或把历史二进制 1–5 一次性迁入相同模型；不维护第二个配置 owner。
pub(crate) fn decode_settings(bytes: &[u8]) -> Result<Configuration, String> {
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{') {
        return Configuration::decode(bytes);
    }
    decode_legacy(bytes).map_err(|_| "invalid historical network configuration".to_string())
}

// 以下只定义历史磁盘字节布局；不能调用会随当前线版本演进的 config/search_list 解码。
fn legacy_config(decoder: &mut Decoder<'_>, with_suffix: bool) -> Result<InterfaceConfig, Invalid> {
    let method = match decoder.u8()? {
        0 => Method::Disabled,
        1 => Method::Dhcp,
        2 => Method::Static,
        _ => return Err(Invalid),
    };
    let address = decoder.address()?;
    let prefix_length = decoder.u8()?;
    let gateway = decoder.address()?;
    let count = usize::from(decoder.u8()?);
    if count > 3 {
        return Err(Invalid);
    }
    let dns = (0..count)
        .map(|_| decoder.address())
        .collect::<Result<Vec<_>, _>>()?;
    let config = InterfaceConfig {
        method,
        address,
        prefix_length,
        gateway,
        dns,
        dns_suffix: if with_suffix {
            dns_suffix(decoder.bytes()?)?.into()
        } else {
            String::new()
        },
    };
    config.validate()?;
    Ok(config)
}

fn decode_legacy(bytes: &[u8]) -> Result<Configuration, Invalid> {
    let mut decoder = Decoder::new(bytes);
    let version = decoder.u8()?;
    if !(1..=SETTINGS_VERSION).contains(&version) {
        return Err(Invalid);
    }
    let count = decoder.u32()? as usize;
    if count > bytes.len() / 16 {
        return Err(Invalid);
    }
    let mut settings = Configuration::default();
    for _ in 0..count {
        let name = decoder.bytes()?;
        let config = legacy_config(&mut decoder, version >= 3)?;
        let profile = settings.ensure_interface(name, None)?;
        if profile.ipv4.is_some() {
            return Err(Invalid);
        }
        profile.enabled = Some(config.method != Method::Disabled);
        profile.ipv4 = Some(Ipv4Configuration::from_projection(&config));
    }
    if version >= 2 {
        let count = decoder.u32()? as usize;
        if count > bytes.len() / 10 {
            return Err(Invalid);
        }
        for _ in 0..count {
            let name = decoder.bytes()?;
            let label = connection_name(decoder.bytes()?)?;
            let profile = settings.ensure_interface(name, None)?;
            if profile.display_name.is_some() {
                return Err(Invalid);
            }
            profile.display_name = Some(label.into());
        }
    }
    if version >= 4 {
        let count = usize::from(decoder.u8()?);
        for _ in 0..count {
            settings
                .dns
                .search_list
                .push(dns_suffix(decoder.bytes()?)?.into());
        }
    }
    if version >= 5 {
        settings.dns.primary_suffix = dns_suffix(decoder.bytes()?)?.into();
        settings.dns.append_parent_suffixes = match decoder.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Invalid),
        };
    }
    decoder.finish()?;
    settings.validate().map_err(|_| Invalid)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_protocol::{Encoder, configuration::DeviceMatch};

    #[test]
    fn configuration_migrates_each_historical_layout_without_reencoding_it() {
        // 固定历史 v1 配置项：eth0、static 10.0.0.2/24、网关 .1、DNS .53。
        // 后续版本只在规定位置添加字段，不调用当前 config/search_list 编码。
        let base = [
            1, 1, 0, 0, 0, 4, 0, 0, 0, b'e', b't', b'h', b'0', 2, 10, 0, 0, 2, 24, 10, 0, 0, 1, 1,
            10, 0, 0, 53,
        ];
        for version in 1..=5 {
            let mut bytes = Encoder(base.to_vec());
            bytes.0[0] = version;
            if version >= 3 {
                bytes.bytes(b"office.test");
            }
            if version >= 2 {
                bytes.u32(1).bytes(b"eth0").bytes(b"Office");
            }
            if version >= 4 {
                bytes.u8(1).bytes(b"corp.test");
            }
            if version >= 5 {
                bytes.bytes(b"primary.test").u8(0);
            }
            let migrated = decode_settings(&bytes.0).unwrap();
            let effective = resolve(&migrated, &[], b"eth0", [2, 0, 0, 0, 0, 1])
                .unwrap()
                .0;
            assert_eq!(effective.method, Method::Static);
            assert_eq!(effective.address, Ipv4Addr::new(10, 0, 0, 2));
            assert_eq!(effective.dns, [Ipv4Addr::new(10, 0, 0, 53)]);
            assert_eq!(
                effective.dns_suffix,
                if version >= 3 { "office.test" } else { "" }
            );
            assert_eq!(migrated.dns.search_list.is_empty(), version < 4);
            assert_eq!(migrated.dns.append_parent_suffixes, version < 5);
            let current = encode_settings(&migrated).unwrap();
            assert_eq!(current.first(), Some(&b'{'));
            assert_eq!(decode_settings(&current).unwrap(), migrated);
            bytes.0.push(0);
            assert!(decode_settings(&bytes.0).is_err());
        }
    }

    #[test]
    fn configuration_name_only_migration_preserves_default_inheritance_and_binds_selected_device() {
        let mut bytes = Encoder::default();
        bytes.u8(2).u32(0).u32(1).bytes(b"eth0").bytes(b"Office");
        let mut migrated = decode_settings(&bytes.0).unwrap();
        let mac = [2, 0, 0, 0, 0, 1];
        let defaults = parse_defaults(b"iface * inet static\naddress 10.0.0.9\nnetmask 255.255.255.0\ndns-nameservers 1.1.1.1 2.2.2.2 3.3.3.3 4.4.4.4\n");
        let expected = resolve(&migrated, &defaults, b"eth0", mac).unwrap();
        assert_eq!(expected.0.address, Ipv4Addr::new(10, 0, 0, 9));
        assert_eq!(expected.0.dns.len(), 4);
        let profile = migrated.ensure_interface(b"eth0", Some(mac)).unwrap();
        assert_eq!(profile.device, DeviceMatch::Mac(mac));
        assert!(profile.ipv4.is_none());
        profile.mtu = 1200;
        assert_eq!(resolve(&migrated, &defaults, b"eth7", mac).unwrap().1, 1200);
        migrated.ensure_interface(b"eth0", Some(mac)).unwrap().mtu = DEFAULT_MTU;
        assert_eq!(
            resolve(&migrated, &defaults, b"eth7", mac).unwrap(),
            expected
        );
        assert!(decode_settings(b"{ broken configuration").is_err());
    }
}
