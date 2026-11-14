//! 配置来源：bootfs 只读默认策略（interfaces(5) 子集）与私有数据目录中的用户设置。

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::net::Ipv4Addr;

use router_protocol::{Decoder, InterfaceConfig, Invalid, Method, connection_name, dns_suffix};

use router_protocol::administration::CommitId;
use router_protocol::configuration::{Configuration, DEFAULT_MTU, Ipv4Configuration};

/// 配置对象的稳定存储身份；格式迁移不创建第二份配置文件。
pub(crate) const SETTINGS_FILE: &[u8] = b"interfaces";
/// 版本 2 至 5 分别添加连接名称、接口 DNS 后缀、全局搜索列表和主后缀设置。
const SETTINGS_VERSION: u8 = 5;
/// 保留最近成功保存的导入身份；限制私有状态随长期使用的增长。
pub(crate) const COMMIT_HISTORY_LIMIT: usize = 128;
// 私有磁盘文件尾部，NUL 使它不能与普通 JSON 或历史正文尾部混淆。
const SETTINGS_FOOTER: &[u8] = b"\0LITEOS-ROUTER\x01";

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
    if saved.routing_domain_for_mac(mac).is_some() {
        // 持久提交前可逆地停止默认域的接口。提交后由设备发现按新的归属重装。
        let mut disabled = InterfaceConfig::dhcp();
        disabled.method = Method::Disabled;
        return Ok((disabled, DEFAULT_MTU));
    }
    let inherited = defaults
        .iter()
        .find(|(entry, _)| entry.as_slice() == name)
        .or_else(|| defaults.iter().find(|(entry, _)| entry.as_slice() == b"*"))
        .map_or_else(InterfaceConfig::dhcp, |(_, config)| config.clone());
    Ok(match saved.matching(name, mac)? {
        Some(profile) if saved.bridged_profile(name, mac).is_some() => {
            let mut port = InterfaceConfig::dhcp();
            port.method = Method::Disabled;
            (port, profile.mtu)
        }
        Some(profile) => (profile.project(inherited), profile.mtu),
        None => (inherited, DEFAULT_MTU),
    })
}

/// 同一私有文件写入配置 JSON 和有界提交记录；导出与备份仍是纯配置 JSON。
pub(crate) fn encode_settings(
    settings: &Configuration,
    commits: &[CommitId],
) -> Result<Vec<u8>, Invalid> {
    if commits.len() > COMMIT_HISTORY_LIMIT {
        return Err(Invalid);
    }
    let mut bytes = settings.encode().map_err(|_| Invalid)?;
    bytes
        .try_reserve_exact(commits.len() * CommitId::BYTES + 1 + SETTINGS_FOOTER.len())
        .map_err(|_| Invalid)?;
    for commit in commits {
        bytes.extend_from_slice(commit.as_bytes());
    }
    bytes.push(commits.len() as u8);
    bytes.extend_from_slice(SETTINGS_FOOTER);
    Ok(bytes)
}

/// 恢复同一文件中的配置与提交记录。没有提交记录的旧文件只在读取时迁移。
pub(crate) fn decode_settings(bytes: &[u8]) -> Result<(Configuration, Vec<CommitId>), String> {
    if let Some(body) = bytes.strip_suffix(SETTINGS_FOOTER) {
        let invalid = || "invalid configuration commit history".to_string();
        let (&count, data) = body.split_last().ok_or_else(invalid)?;
        let count = usize::from(count);
        if count > COMMIT_HISTORY_LIMIT {
            return Err(invalid());
        }
        let offset = data
            .len()
            .checked_sub(count * CommitId::BYTES)
            .ok_or_else(invalid)?;
        let (configuration, records) = data.split_at(offset);
        let mut commits = Vec::new();
        commits.try_reserve_exact(count).map_err(|_| invalid())?;
        for bytes in records.as_chunks::<{ CommitId::BYTES }>().0 {
            let id = CommitId::from_bytes(bytes).map_err(|_| invalid())?;
            if commits.contains(&id) {
                return Err(invalid());
            }
            commits.push(id);
        }
        return Ok((Configuration::decode(configuration)?, commits));
    }
    decode_document(bytes).map(|configuration| (configuration, Vec::new()))
}

/// 导入当前 JSON 或旧版网络设置副本，返回当前配置模型。
///
/// # Errors
/// 文档格式或配置语义无效。私有持久化尾部只由启动读取器处理。
pub(crate) fn decode_document(bytes: &[u8]) -> Result<Configuration, String> {
    match bytes.first() {
        Some(1..=SETTINGS_VERSION) => {
            decode_legacy(bytes).map_err(|_| "invalid historical network configuration".to_string())
        }
        _ => Configuration::decode(bytes),
    }
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
            let (migrated, commits) = decode_settings(&bytes.0).unwrap();
            assert!(commits.is_empty());
            assert_eq!(decode_document(&bytes.0).unwrap(), migrated);
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
            let commits = vec![CommitId::from_random_bytes([version; 16])];
            let current = encode_settings(&migrated, &commits).unwrap();
            assert_eq!(current.first(), Some(&b'{'));
            assert_eq!(decode_settings(&current).unwrap(), (migrated, commits));
            assert!(decode_document(&current).is_err());
            bytes.0.push(0);
            assert!(decode_settings(&bytes.0).is_err());
            assert!(decode_document(&bytes.0).is_err());
        }
    }

    #[test]
    fn configuration_name_only_migration_preserves_default_inheritance_and_binds_selected_device() {
        let mut bytes = Encoder::default();
        bytes.u8(2).u32(0).u32(1).bytes(b"eth0").bytes(b"Office");
        let (mut migrated, _) = decode_settings(&bytes.0).unwrap();
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
