//! 有界的通告 DNS 列表，在配置事务中保持按值交换，不在激活或回滚时分配。

use super::*;
use core::net::Ipv6Addr;
use serde::{de::SeqAccess, ser::SerializeSeq};

/// 单个接口的 DNS 通告最多携带八个服务器，保留配置中的优先顺序。
pub const MAX_ADVERTISED_DNS: usize = 8;

/// RA 中显式发布的递归 DNS 服务器。空列表只发布地址和路由信息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvertisedDns {
    addresses: [Ipv6Addr; MAX_ADVERTISED_DNS],
    length: u8,
}

impl Default for AdvertisedDns {
    fn default() -> Self {
        Self {
            addresses: [Ipv6Addr::UNSPECIFIED; MAX_ADVERTISED_DNS],
            length: 0,
        }
    }
}

impl AdvertisedDns {
    /// 检查非重复 IPv6 单播服务器并保留原顺序，链路本地地址使用发送接口的作用域。
    /// # Errors
    /// 列表超过容量或包含不可发布、重复的地址。
    pub fn new(addresses: &[Ipv6Addr]) -> Result<Self, Invalid> {
        let mut result = Self::default();
        for &address in addresses {
            result.push(address)?;
        }
        Ok(result)
    }

    fn push(&mut self, address: Ipv6Addr) -> Result<(), Invalid> {
        if address.is_unspecified()
            || address.is_multicast()
            || address.is_loopback()
            || address.to_ipv4_mapped().is_some()
            || self.as_slice().contains(&address)
        {
            return Err(Invalid);
        }
        *self
            .addresses
            .get_mut(usize::from(self.length))
            .ok_or(Invalid)? = address;
        self.length += 1;
        Ok(())
    }

    /// 借用已检查、按优先顺序排列的服务器。
    pub fn as_slice(&self) -> &[Ipv6Addr] {
        &self.addresses[..usize::from(self.length)]
    }
}

/// RDNSS 选项占用的报文字节数，空集合不产生选项。count 来自有界的通告列表。
pub const fn rdnss_size(count: usize) -> usize {
    if count == 0 { 0 } else { 8 + 16 * count }
}

impl Serialize for AdvertisedDns {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(usize::from(self.length)))?;
        for address in self.as_slice() {
            sequence.serialize_element(address)?;
        }
        sequence.end()
    }
}

impl<'de> Deserialize<'de> for AdvertisedDns {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Addresses;
        impl<'de> Visitor<'de> for Addresses {
            type Value = AdvertisedDns;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("at most eight distinct IPv6 DNS server addresses")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut result = AdvertisedDns::default();
                while let Some(address) = sequence.next_element()? {
                    result
                        .push(address)
                        .map_err(|_| de::Error::custom("invalid advertised DNS server"))?;
                }
                Ok(result)
            }
        }
        deserializer.deserialize_seq(Addresses)
    }
}

impl JsonSchema for AdvertisedDns {
    fn schema_name() -> alloc::borrow::Cow<'static, str> {
        "AdvertisedDns".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "array",
            "items": {"type": "string", "format": "ipv6"},
            "maxItems": MAX_ADVERTISED_DNS,
            "uniqueItems": true
        })
    }
}
