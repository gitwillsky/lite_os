//! 一次 NET guard 冻结的生效规则；同一行携带实际设备名字，读取不再回查接口。

use crate::{
    SocketError,
    native_abi::{self, Status},
};
use alloc::vec::Vec;
use router_protocol::{NAME_MAX, policy_routing::Rule, routing::Family};

const VERSION: u32 = 2;
const HEADER: usize = 4;
const ROW: usize = Rule::BYTES + NAME_MAX;

/// 一条生效选表规则和同次捕获的入接口名字。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleEntry {
    pub rule: Rule,
    /// NUL 终止并补零。规则无入接口限制时为全零。
    pub ingress_name: [u8; NAME_MAX],
}

impl RuleEntry {
    /// 返回入接口名字及结尾 NUL；没有接口限制时返回 None。
    pub fn ingress(&self) -> Option<&[u8]> {
        if self.rule.ingress == 0 {
            return None;
        }
        let end = self.ingress_name.iter().position(|byte| *byte == 0)?;
        (end != 0).then_some(&self.ingress_name[..=end])
    }

    fn validate(self, previous: &mut Option<(u8, u32)>) -> Result<(), Status> {
        let family = self.rule.family();
        let valid_name = if self.rule.ingress == 0 {
            self.ingress_name == [0; NAME_MAX]
        } else {
            self.ingress().is_some_and(|name| {
                self.ingress_name[name.len()..]
                    .iter()
                    .all(|byte| *byte == 0)
            })
        };
        let key = (
            match family {
                Family::Ipv4 => 4,
                Family::Ipv6 => 6,
            },
            self.rule.priority,
        );
        if !valid_name
            || !self.rule.valid() && !Rule::implicit(family).contains(&self.rule)
            || previous.is_some_and(|previous| previous >= key)
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        *previous = Some(key);
        Ok(())
    }

    fn decode(row: &[u8; ROW]) -> Result<Self, Status> {
        let (rule, name) = row.split_at(Rule::BYTES);
        Ok(Self {
            rule: Rule::decode(rule.try_into().expect("rule width"))
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            ingress_name: name.try_into().expect("interface name width"),
        })
    }
}

/// 只读规则正文。按 IPv4、IPv6 和各族优先级递增排列，读取单行无需分配。
#[derive(Debug)]
pub struct RuleSnapshot(Vec<u8>);

impl RuleSnapshot {
    /// 捕获同一 NET guard 中的规则及设备名字，迭代顺序和长度在捕获期间必须稳定。
    /// # Errors
    /// 规则非法、顺序错误、长度溢出或资源不足。
    pub fn capture(rows: impl Clone + Iterator<Item = RuleEntry>) -> Result<Self, SocketError> {
        let length = rows
            .clone()
            .count()
            .checked_mul(ROW)
            .and_then(|bytes| HEADER.checked_add(bytes))
            .ok_or(SocketError::Invalid)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| SocketError::NoMemory)?;
        bytes.resize(length, 0);
        bytes[..HEADER].copy_from_slice(&VERSION.to_le_bytes());
        let mut slots = bytes[HEADER..].as_chunks_mut::<ROW>().0.iter_mut();
        let mut previous = None;
        for row in rows {
            row.validate(&mut previous)
                .map_err(|_| SocketError::Invalid)?;
            let slot = slots.next().ok_or(SocketError::Invalid)?;
            slot[..Rule::BYTES].copy_from_slice(&row.rule.encode());
            slot[Rule::BYTES..].copy_from_slice(&row.ingress_name);
        }
        if slots.next().is_some() {
            return Err(SocketError::Invalid);
        }
        Ok(Self(bytes))
    }

    /// 接管并验证完整只读正文。
    /// # Errors
    /// 未知版本、截断、非法规则或顺序错误。
    pub fn parse(bytes: Vec<u8>) -> Result<Self, Status> {
        let body = bytes
            .strip_prefix(&VERSION.to_le_bytes())
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let (rows, tail) = body.as_chunks::<ROW>();
        if !tail.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut previous = None;
        for row in rows {
            RuleEntry::decode(row)?.validate(&mut previous)?;
        }
        Ok(Self(bytes))
    }

    /// 借用完整版本化正文。
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// 返回冻结规则数。
    pub fn len(&self) -> usize {
        (self.0.len() - HEADER) / ROW
    }

    /// 返回是否没有规则。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 读取一条已验证的规则，越界时返回 None。
    pub fn get(&self, index: usize) -> Option<RuleEntry> {
        let row = self.0[HEADER..].as_chunks::<ROW>().0.get(index)?;
        Some(RuleEntry::decode(row).expect("immutable validated rule"))
    }
}
