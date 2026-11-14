//! 配置管理的控制帧。正文经 fd/VMO 传递，长度为 u64，不随内联消息发送。

use crate::{Decoder, Encoder, Invalid, Revision, ordinal};
use alloc::vec::Vec;

mod commit;
mod domain;
pub use commit::{CommitId, CommitResult};
pub use domain::{domain_names, parse_domain_names, parse_scoped_query, scoped_query};

const VERSION: u32 = 11;

fn accepts_domain(operation: u32) -> bool {
    matches!(
        operation,
        ordinal::FIREWALL_COUNTERS
            | ordinal::CONNECTIONS
            | ordinal::CLEAR_CONNECTION
            | ordinal::CONNECTION_CHANGES
            | ordinal::NEIGHBOR_PAGE
            | ordinal::BGP_STATUS
            | ordinal::BGP_ROUTES
            | ordinal::BGP_REFRESH
            | ordinal::OSPF_STATUS
            | ordinal::ROUTE_PAGE
            | ordinal::OPEN_NETWORK_NAMESPACE
    )
}

/// 管理请求的完整元数据；文档能力数必须等于 `usize::from(length != 0)`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Request<'a> {
    /// 只有备份导出/删除携带固定身份，不接受路径。
    pub backup: Option<crate::backup::Id>,
    /// 只有整份导入携带预期代际。
    pub generation: Option<Revision>,
    /// 导入及结果查询携带提交身份。
    pub commit: Option<CommitId>,
    /// 文档的逻辑字节数，不含 VMO 尾页填充。
    pub length: u64,
    /// 管理操作的具名域范围，None 表示默认域。
    pub domain: Option<&'a str>,
}

impl<'a> Request<'a> {
    fn validate(self, operation: u32) -> Result<(), Invalid> {
        let document = matches!(
            operation,
            ordinal::VALIDATE
                | ordinal::IMPORT
                | ordinal::DIFF
                | ordinal::BGP_ROUTES
                | ordinal::BGP_REFRESH
                | ordinal::DHCP_SERVER_BINDINGS
                | ordinal::CONNECTIONS
                | ordinal::CLEAR_CONNECTION
                | ordinal::CONNECTION_CHANGES
                | ordinal::FIREWALL_COUNTERS
                | ordinal::NEIGHBOR_PAGE
                | ordinal::ROUTE_PAGE
        );
        if !ordinal::is_configuration(operation)
            || self.backup.is_some()
                != matches!(operation, ordinal::BACKUP_EXPORT | ordinal::BACKUP_DELETE)
            || self.generation.is_some() != (operation == ordinal::IMPORT)
            || self.commit.is_some()
                != matches!(operation, ordinal::IMPORT | ordinal::COMMIT_RESULT)
            || (self.length != 0) != document
            || (!matches!(
                operation,
                ordinal::VALIDATE | ordinal::IMPORT | ordinal::DIFF
            ) && self.length > crate::bridge::DATAGRAM_MAX as u64)
            || (matches!(
                operation,
                ordinal::CONNECTIONS | ordinal::CLEAR_CONNECTION | ordinal::CONNECTION_CHANGES
            ) && self.length != crate::connections::CURSOR_BYTES as u64)
            || (operation == ordinal::FIREWALL_COUNTERS
                && self.length != crate::firewall::counters::CURSOR_BYTES as u64)
            || (operation == ordinal::NEIGHBOR_PAGE
                && self.length != crate::neighbors::CURSOR_BYTES as u64)
            || (operation == ordinal::ROUTE_PAGE
                && self.length != crate::route_view::CURSOR_BYTES as u64)
            || self.domain.is_some_and(|domain| {
                !accepts_domain(operation)
                    || !crate::configuration::valid_routing_domain_name(domain)
            })
            || (operation == ordinal::OPEN_NETWORK_NAMESPACE && self.domain.is_none())
        {
            return Err(Invalid);
        }
        Ok(())
    }
    /// 编码控制帧；文档本体不参与编码。
    /// # Errors
    /// 操作、代际或文档存在性不符。
    pub fn encode(self, operation: u32) -> Result<Vec<u8>, Invalid> {
        self.validate(operation)?;
        let mut encoded = Encoder::default();
        encoded.u32(VERSION);
        if let Some(generation) = self.generation {
            encoded.0.extend_from_slice(&generation.to_bytes());
        }
        if let Some(id) = self.commit {
            encoded.0.extend_from_slice(id.as_bytes());
        }
        if let Some(backup) = self.backup {
            encoded.bytes(&backup.to_bytes());
        }
        encoded.u64(self.length);
        if accepts_domain(operation) {
            encoded.bytes(self.domain.unwrap_or_default().as_bytes());
        }
        Ok(encoded.0)
    }
    /// 解码并完整校验控制帧；未知版本或尾部不能触发业务副作用。
    /// # Errors
    /// 字段截断、尾部多余、版本或操作约束不符。
    pub fn decode(operation: u32, bytes: &'a [u8]) -> Result<Self, Invalid> {
        let mut decoder = Decoder::new(bytes);
        if decoder.u32()? != VERSION {
            return Err(Invalid);
        }
        let generation = if operation == ordinal::IMPORT {
            Some(Revision::from_bytes(decoder.take(Revision::BYTES)?)?)
        } else {
            None
        };
        let commit = if matches!(operation, ordinal::IMPORT | ordinal::COMMIT_RESULT) {
            Some(CommitId::from_bytes(decoder.take(CommitId::BYTES)?)?)
        } else {
            None
        };
        let backup = if matches!(operation, ordinal::BACKUP_EXPORT | ordinal::BACKUP_DELETE) {
            Some(crate::backup::Id::from_bytes(
                decoder.bytes()?.try_into().map_err(|_| Invalid)?,
            ))
        } else {
            None
        };
        let length = decoder.u64()?;
        let domain = if accepts_domain(operation) {
            let name = core::str::from_utf8(decoder.bytes()?).map_err(|_| Invalid)?;
            (!name.is_empty()).then_some(name)
        } else {
            None
        };
        let request = Self {
            backup,
            generation,
            commit,
            length,
            domain,
        };
        decoder.finish()?;
        request.validate(operation)?;
        Ok(request)
    }
}

/// 管理回复的元数据；无正文的操作只确认结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    /// 操作所观察或提交的代际，含服务实例身份。
    pub generation: Revision,
    /// 文档逻辑长度；无正文的确认必须为零。
    pub length: u64,
}
impl Reply {
    /// 固定回复正文长度，不含传输控制字段。
    pub const BYTES: usize = 4 + Revision::BYTES + 8;

    /// 当前操作的成功回复是否携带内容能力；零字节备份仍必须与无正文确认区分。
    pub fn has_document(operation: u32) -> bool {
        ordinal::is_configuration(operation)
            && !matches!(
                operation,
                ordinal::IMPORT
                    | ordinal::BACKUP_DELETE
                    | ordinal::BGP_REFRESH
                    | ordinal::CLEAR_CONNECTION
                    | ordinal::OPEN_NETWORK_NAMESPACE
            )
    }

    /// 编码固定长度的回复控制帧。
    pub fn encode(self) -> [u8; Self::BYTES] {
        let mut bytes = [0; Self::BYTES];
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..4 + Revision::BYTES].copy_from_slice(&self.generation.to_bytes());
        bytes[4 + Revision::BYTES..].copy_from_slice(&self.length.to_le_bytes());
        bytes
    }
    /// 解码并校验当前操作的回复；能力数另由传输端校验。
    /// # Errors
    /// 字段、版本、零代际、无正文确认的长度或操作不合法。
    pub fn decode(operation: u32, bytes: &[u8]) -> Result<Self, Invalid> {
        let mut decoder = Decoder::new(bytes);
        if decoder.u32()? != VERSION {
            return Err(Invalid);
        }
        let reply = Self {
            generation: Revision::from_bytes(decoder.take(Revision::BYTES)?)?,
            length: decoder.u64()?,
        };
        decoder.finish()?;
        if !ordinal::is_configuration(operation)
            || (!Self::has_document(operation) && reply.length != 0)
            || (operation == ordinal::CONNECTIONS
                && !crate::connections::valid_page_length(reply.length))
            || (operation == ordinal::CONNECTION_CHANGES
                && !crate::connections::changes::valid_length(reply.length))
            || (operation == ordinal::FIREWALL_COUNTERS
                && !crate::firewall::counters::valid_length(reply.length))
            || (operation == ordinal::NEIGHBOR_PAGE
                && !crate::neighbors::valid_page_length(reply.length))
            || (operation == ordinal::ROUTE_PAGE
                && !crate::route_view::valid_page_length(reply.length))
            || (operation == ordinal::COMMIT_RESULT && reply.length != 1)
        {
            return Err(Invalid);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    #[test]
    fn configuration_documents_have_fixed_control_frames_and_strict_presence() {
        let generation = Revision::initial(core::num::NonZeroU128::MIN);
        let another_instance = Revision::initial(core::num::NonZeroU128::new(2).unwrap());
        assert_ne!(generation, another_instance);
        assert_eq!(generation.to_string().parse(), Ok(generation));
        assert!("1".parse::<Revision>().is_err());
        let request = Request {
            generation: Some(generation),
            commit: Some(CommitId::from_random_bytes([1; 16])),
            length: u64::from(u32::MAX) + 1,
            ..Request::default()
        };
        let bytes = request.encode(ordinal::IMPORT).unwrap();
        assert_eq!(bytes.len(), 52);
        assert_eq!(Request::decode(ordinal::IMPORT, &bytes), Ok(request));
        let mut invalid = bytes.clone();
        invalid[4..20].fill(0);
        assert!(Request::decode(ordinal::IMPORT, &invalid).is_err());
        assert!(request.encode(ordinal::VALIDATE).is_err());
        assert!(
            Request {
                length: 1,
                ..Request::default()
            }
            .encode(ordinal::EXPORT)
            .is_err()
        );
        let reply = Reply {
            generation: generation.checked_next().unwrap(),
            length: request.length,
        };
        assert_eq!(Reply::decode(ordinal::EXPORT, &reply.encode()), Ok(reply));
        assert!(Reply::decode(ordinal::IMPORT, &reply.encode()).is_err());
        for length in 0..bytes.len() {
            assert!(Request::decode(ordinal::IMPORT, &bytes[..length]).is_err());
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(Request::decode(ordinal::IMPORT, &extra).is_err());

        let backup = Request {
            backup: Some(crate::backup::Id::from_bytes([5; 16])),
            ..Request::default()
        };
        let bytes = backup.encode(ordinal::BACKUP_EXPORT).unwrap();
        assert_eq!(Request::decode(ordinal::BACKUP_EXPORT, &bytes), Ok(backup));
        assert!(backup.encode(ordinal::BACKUPS).is_err());
        assert!(Request::decode(ordinal::BACKUP_EXPORT, &bytes[..bytes.len() - 1]).is_err());
        let deleted = Reply {
            generation: reply.generation.checked_next().unwrap(),
            length: 0,
        };
        assert_eq!(
            Reply::decode(ordinal::BACKUP_DELETE, &deleted.encode()),
            Ok(deleted)
        );
        assert_eq!(
            Reply::decode(ordinal::BACKUP_EXPORT, &deleted.encode()),
            Ok(deleted)
        );
        assert!(Reply::has_document(ordinal::BACKUP_EXPORT));
        assert!(!Reply::has_document(ordinal::BACKUP_DELETE));
        let status = Request::default();
        let counters = Request {
            domain: Some("隔离网络"),
            length: crate::firewall::counters::CURSOR_BYTES as u64,
            ..status
        };
        let encoded = counters.encode(ordinal::FIREWALL_COUNTERS).unwrap();
        assert_eq!(
            Request::decode(ordinal::FIREWALL_COUNTERS, &encoded),
            Ok(counters)
        );
        assert!(
            Request::decode(ordinal::FIREWALL_COUNTERS, &encoded[..encoded.len() - 1]).is_err()
        );
        assert!(
            Request {
                length: 0,
                ..counters
            }
            .encode(ordinal::EXPORT)
            .is_err()
        );
        let connection = Request {
            length: crate::connections::CURSOR_BYTES as u64,
            ..counters
        };
        let encoded = connection.encode(ordinal::CLEAR_CONNECTION).unwrap();
        assert_eq!(
            Request::decode(ordinal::CLEAR_CONNECTION, &encoded),
            Ok(connection)
        );
        // 控制帧缺失域字段时必须拒绝，不能按默认域执行连接清除。
        assert!(Request::decode(ordinal::CLEAR_CONNECTION, &encoded[..12]).is_err());
        let cursor = crate::connections::changes::Cursor::default().encode();
        let wait = scoped_query(connection.domain, &cursor).unwrap();
        assert_eq!(
            parse_scoped_query(&wait),
            Ok((connection.domain, cursor.as_slice()))
        );
        assert!(parse_scoped_query(&cursor).is_err());
        let open_namespace = Request {
            domain: Some("隔离网络"),
            ..status
        };
        let ordinal = ordinal::OPEN_NETWORK_NAMESPACE;
        let encoded = open_namespace.encode(ordinal).unwrap();
        assert_eq!(Request::decode(ordinal, &encoded), Ok(open_namespace));
        assert!(status.encode(ordinal).is_err());
        assert!(!Reply::has_document(ordinal));
        let receipt = Request {
            commit: request.commit,
            ..status
        };
        let bytes = receipt.encode(ordinal::COMMIT_RESULT).unwrap();
        assert_eq!(Request::decode(ordinal::COMMIT_RESULT, &bytes), Ok(receipt));
        assert!(Request::decode(ordinal::IMPORT, &bytes).is_err());
        assert!(status.encode(ordinal::COMMIT_RESULT).is_err());
        assert_eq!(
            Request::decode(
                ordinal::BGP_STATUS,
                &status.encode(ordinal::BGP_STATUS).unwrap()
            ),
            Ok(status)
        );
        assert!(request.encode(ordinal::BGP_STATUS).is_err());
        let query = crate::tracking::IpPrefix {
            network: "198.51.100.0".parse().unwrap(),
            length: 24,
        };
        let json = query.encode().unwrap();
        assert_eq!(crate::tracking::IpPrefix::parse(&json), Ok(query));
        assert!(
            crate::tracking::IpPrefix {
                network: "198.51.100.1".parse().unwrap(),
                ..query
            }
            .encode()
            .is_err()
        );
        assert!(
            crate::tracking::IpPrefix {
                length: 33,
                ..query
            }
            .encode()
            .is_err()
        );
        assert!(
            crate::tracking::IpPrefix::parse(
                br#"{"network":"198.51.100.0","length":24,"extra":true}"#
            )
            .is_err()
        );
        assert!(status.encode(ordinal::BGP_ROUTES).is_err());
        let lookup = Request {
            length: json.len() as u64,
            ..status
        };
        assert_eq!(
            Request::decode(
                ordinal::BGP_ROUTES,
                &lookup.encode(ordinal::BGP_ROUTES).unwrap()
            ),
            Ok(lookup)
        );
        assert!(Reply::has_document(ordinal::BGP_ROUTES));

        let refresh =
            crate::bgp::RefreshRequest::parse(r#"{"neighbor":"上联","family":"ipv6"}"#.as_bytes())
                .unwrap();
        let document = refresh.encode().unwrap();
        assert_eq!(crate::bgp::RefreshRequest::parse(&document), Ok(refresh));
        assert!(crate::bgp::RefreshRequest::parse(br#"{"neighbor":"","family":"ipv4"}"#).is_err());
        assert!(
            crate::bgp::RefreshRequest::parse(br#"{"neighbor":"upstream","family":"any"}"#)
                .is_err()
        );
        let command = Request {
            length: document.len() as u64,
            ..status
        };
        assert!(status.encode(ordinal::BGP_REFRESH).is_err());
        assert_eq!(
            Request::decode(
                ordinal::BGP_REFRESH,
                &command.encode(ordinal::BGP_REFRESH).unwrap()
            ),
            Ok(command)
        );
        assert!(!Reply::has_document(ordinal::BGP_REFRESH));
        assert!(Reply::decode(ordinal::BGP_REFRESH, &reply.encode()).is_err());
        let accepted = Reply { length: 0, ..reply };
        assert_eq!(
            Reply::decode(ordinal::BGP_REFRESH, &accepted.encode()),
            Ok(accepted)
        );

        assert!(Reply::has_document(ordinal::BGP_STATUS));
        assert_eq!(
            Reply::decode(ordinal::BGP_STATUS, &reply.encode()),
            Ok(reply)
        );
    }
}
