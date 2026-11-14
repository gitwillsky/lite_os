//! 供授权管理会话选择路由域的名称目录和内联查询。

use crate::{Decoder, Encoder, Invalid, configuration::valid_routing_domain_name};
use alloc::{string::String, vec::Vec};

const VERSION: u32 = 1;

/// 编码具名域 `domain` 或默认域及其内联游标 `query`，供有界等待或订阅使用。
/// # Errors
/// 域名无效或查询超出控制帧容量。
pub fn scoped_query(domain: Option<&str>, query: &[u8]) -> Result<Vec<u8>, Invalid> {
    let name = domain.unwrap_or_default();
    if domain.is_some_and(|name| !valid_routing_domain_name(name))
        || query.len() > crate::bridge::DATAGRAM_MAX - crate::bridge::HEADER_BYTES - 12 - name.len()
    {
        return Err(Invalid);
    }
    let mut control = Encoder::default();
    control
        .0
        .try_reserve_exact(12 + name.len() + query.len())
        .map_err(|_| Invalid)?;
    control.u32(VERSION).bytes(name.as_bytes()).bytes(query);
    Ok(control.0)
}

/// 解码域及其查询正文。调用者按操作进一步解码正文。
/// # Errors
/// 版本、域名、长度或尾部无效。
pub fn parse_scoped_query(control: &[u8]) -> Result<(Option<&str>, &[u8]), Invalid> {
    if control.len() > crate::bridge::DATAGRAM_MAX - crate::bridge::HEADER_BYTES {
        return Err(Invalid);
    }
    let mut decoder = Decoder::new(control);
    if decoder.u32()? != VERSION {
        return Err(Invalid);
    }
    let name = core::str::from_utf8(decoder.bytes()?).map_err(|_| Invalid)?;
    let domain = (!name.is_empty()).then_some(name);
    if domain.is_some_and(|name| !valid_routing_domain_name(name)) {
        return Err(Invalid);
    }
    let query = decoder.bytes()?;
    decoder.finish()?;
    Ok((domain, query))
}

/// 编码按名称升序排列的具名域 `names`，空目录仍携带版本。
/// # Errors
/// 名称无效、重复、顺序不合法或无法分配正文。
pub fn domain_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Vec<u8>, Invalid> {
    let mut document = Encoder::default();
    document.0.try_reserve_exact(4).map_err(|_| Invalid)?;
    document.u32(VERSION);
    let mut previous = None;
    for name in names {
        if !valid_routing_domain_name(name) || previous.is_some_and(|previous| previous >= name) {
            return Err(Invalid);
        }
        document
            .0
            .try_reserve(4 + name.len())
            .map_err(|_| Invalid)?;
        document.bytes(name.as_bytes());
        previous = Some(name);
    }
    Ok(document.0)
}

/// 解码只含具名域名称的授权目录 `document`。
/// # Errors
/// 版本、名称、顺序或正文长度无效，或无法分配结果。
pub fn parse_domain_names(document: &[u8]) -> Result<Vec<String>, Invalid> {
    let mut decoder = Decoder::new(document);
    if decoder.u32()? != VERSION {
        return Err(Invalid);
    }
    let mut names = Vec::<String>::new();
    while decoder.finish().is_err() {
        let name = core::str::from_utf8(decoder.bytes()?).map_err(|_| Invalid)?;
        if !valid_routing_domain_name(name)
            || names.last().is_some_and(|last| last.as_str() >= name)
        {
            return Err(Invalid);
        }
        let mut owned = String::new();
        owned.try_reserve_exact(name.len()).map_err(|_| Invalid)?;
        owned.push_str(name);
        names.try_reserve(1).map_err(|_| Invalid)?;
        names.push(owned);
    }
    Ok(names)
}
