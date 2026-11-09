//! 配置管理的控制帧。JSON 文档一律经 fd/VMO 传递，长度为 u64，不随内联消息发送。

use crate::{Decoder, Encoder, Invalid, VERSION, ordinal};
use alloc::vec::Vec;

/// 管理请求的完整元数据；文档能力数必须等于 `usize::from(length != 0)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// 只有备份导出/删除携带固定身份，不接受路径。
    pub backup: Option<crate::backup::Id>,
    /// 只有整份导入携带预期代际。
    pub generation: Option<u64>,
    /// 文档的逻辑字节数，不含 VMO 尾页填充。
    pub length: u64,
}

impl Request {
    fn validate(self, operation: u32) -> Result<(), Invalid> {
        let document = matches!(
            operation,
            ordinal::VALIDATE | ordinal::IMPORT | ordinal::DIFF
        );
        if !ordinal::is_configuration(operation)
            || self.backup.is_some()
                != matches!(operation, ordinal::BACKUP_EXPORT | ordinal::BACKUP_DELETE)
            || self.generation.is_some() != (operation == ordinal::IMPORT)
            || (self.length != 0) != document
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
            encoded.u64(generation);
        }
        if let Some(backup) = self.backup {
            encoded.bytes(&backup.to_bytes());
        }
        encoded.u64(self.length);
        Ok(encoded.0)
    }
    /// 解码并完整校验控制帧；未知版本或尾部不能触发业务副作用。
    /// # Errors
    /// 字段截断、尾部多余、版本或操作约束不符。
    pub fn decode(operation: u32, bytes: &[u8]) -> Result<Self, Invalid> {
        let mut decoder = Decoder::new(bytes);
        decoder.version()?;
        let generation = if operation == ordinal::IMPORT {
            Some(decoder.u64()?)
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
        let request = Self {
            backup,
            generation,
            length: decoder.u64()?,
        };
        decoder.finish()?;
        request.validate(operation)?;
        Ok(request)
    }
}

/// 管理回复的元数据；IMPORT/备份删除没有文档，其他操作各带一个内容能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    /// 操作所观察/提交的服务实例内代际。
    pub generation: u64,
    /// 文档逻辑长度；内容可为空，IMPORT/备份删除确认必须为零。
    pub length: u64,
}
impl Reply {
    /// 当前操作的成功回复是否携带内容能力；零字节备份仍必须与无正文确认区分。
    pub fn has_document(operation: u32) -> bool {
        ordinal::is_configuration(operation)
            && !matches!(operation, ordinal::IMPORT | ordinal::BACKUP_DELETE)
    }

    /// 固定 20-byte 的回复控制帧。
    pub fn encode(self) -> [u8; 20] {
        let mut bytes = [0; 20];
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..12].copy_from_slice(&self.generation.to_le_bytes());
        bytes[12..].copy_from_slice(&self.length.to_le_bytes());
        bytes
    }
    /// 解码并校验当前操作的回复；能力数另由传输端校验。
    /// # Errors
    /// 字段、版本、零代际、无正文确认的长度或操作不合法。
    pub fn decode(operation: u32, bytes: &[u8]) -> Result<Self, Invalid> {
        let mut decoder = Decoder::new(bytes);
        decoder.version()?;
        let reply = Self {
            generation: decoder.u64()?,
            length: decoder.u64()?,
        };
        decoder.finish()?;
        if !ordinal::is_configuration(operation)
            || reply.generation == 0
            || (!Self::has_document(operation) && reply.length != 0)
        {
            return Err(Invalid);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_documents_have_fixed_control_frames_and_strict_presence() {
        let request = Request {
            backup: None,
            generation: Some(17),
            length: u64::from(u32::MAX) + 1,
        };
        let bytes = request.encode(ordinal::IMPORT).unwrap();
        assert_eq!(bytes.len(), 20);
        assert_eq!(Request::decode(ordinal::IMPORT, &bytes), Ok(request));
        assert!(request.encode(ordinal::VALIDATE).is_err());
        assert!(
            Request {
                backup: None,
                generation: None,
                length: 1
            }
            .encode(ordinal::EXPORT)
            .is_err()
        );
        let reply = Reply {
            generation: 18,
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
            generation: None,
            length: 0,
        };
        let bytes = backup.encode(ordinal::BACKUP_EXPORT).unwrap();
        assert_eq!(Request::decode(ordinal::BACKUP_EXPORT, &bytes), Ok(backup));
        assert!(backup.encode(ordinal::BACKUPS).is_err());
        assert!(Request::decode(ordinal::BACKUP_EXPORT, &bytes[..bytes.len() - 1]).is_err());
        let deleted = Reply {
            generation: 19,
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
    }
}
