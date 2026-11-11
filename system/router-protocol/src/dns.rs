//! 通告学到的 DNS 运行时投影。地址保留 IPv6 链路范围，寿命归协议栈。
use super::{Decoder, Encoder, Invalid, dns_suffix};
use alloc::{string::String, vec::Vec};
use core::net::{Ipv6Addr, SocketAddrV6};

/// 域查询投影上限，独立于 libc 和每链路的学习上限。
pub const MAX_LEARNED: usize = 32;
/// 完整线快照字节上限，在接受 VMO 长度前校验。
pub const MAX_ENCODED_BYTES: usize = 12 + MAX_LEARNED * (20 + 4 + super::DNS_SUFFIX_MAX);
/// DNS 快照的版本。设置快照嵌入同一格式。
pub const VERSION: u32 = 1;

/// 尚有效的服务器和搜索域，依学习顺序输出。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LearnedDns {
    pub servers: Vec<SocketAddrV6>,
    pub domains: Vec<String>,
}

/// 验证通告可发布的单播 DNS 地址。
pub fn is_server(address: Ipv6Addr) -> bool {
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && address.to_ipv4_mapped().is_none()
}

impl LearnedDns {
    /// 在网络栈查询路径预留完整编码空间。
    pub fn encoded_len(&self) -> usize {
        12 + 20 * self.servers.len()
            + self
                .domains
                .iter()
                .map(|name| 4 + name.len())
                .sum::<usize>()
    }
}

impl Encoder {
    /// 编码已验证的 DNS 投影。
    pub fn learned_dns(&mut self, dns: &LearnedDns) -> &mut Self {
        self.u32(VERSION).u32(dns.servers.len() as u32);
        for server in &dns.servers {
            self.0.extend_from_slice(&server.ip().octets());
            self.u32(server.scope_id());
        }
        self.u32(dns.domains.len() as u32);
        for domain in &dns.domains {
            self.bytes(domain.as_bytes());
        }
        self
    }
}

impl Decoder<'_> {
    /// 解码 DNS 投影。版本、大小、无效地址、作用域或搜索域不合法时返回 Invalid。
    pub fn learned_dns(&mut self) -> Result<LearnedDns, Invalid> {
        if self.u32()? != VERSION {
            return Err(Invalid);
        }
        let servers = self.u32()? as usize;
        if servers > MAX_LEARNED {
            return Err(Invalid);
        }
        let mut dns = LearnedDns::default();
        dns.servers
            .try_reserve_exact(servers)
            .map_err(|_| Invalid)?;
        for _ in 0..servers {
            let address =
                Ipv6Addr::from(<[u8; 16]>::try_from(self.take(16)?).map_err(|_| Invalid)?);
            let scope = self.u32()?;
            if !is_server(address) || address.is_unicast_link_local() != (scope != 0) {
                return Err(Invalid);
            }
            dns.servers.push(SocketAddrV6::new(address, 53, 0, scope));
        }
        let domains = self.u32()? as usize;
        if domains > MAX_LEARNED {
            return Err(Invalid);
        }
        dns.domains
            .try_reserve_exact(domains)
            .map_err(|_| Invalid)?;
        for _ in 0..domains {
            let domain = dns_suffix(self.bytes()?)?;
            if domain.is_empty() {
                return Err(Invalid);
            }
            let mut name = String::new();
            name.try_reserve_exact(domain.len()).map_err(|_| Invalid)?;
            name.push_str(domain);
            dns.domains.push(name);
        }
        Ok(dns)
    }
}
