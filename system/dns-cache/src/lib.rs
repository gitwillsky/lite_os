#![no_std]
#![feature(allocator_ext)]

//! Forwarding query validation and bounded DNS response caching for one upstream policy.

extern crate alloc;

mod cache;
pub use cache::Cache;

use packet_wire::{
    PacketError,
    dns::{
        FLAG_AD, FLAG_CD, FLAG_QR, FLAG_RA, FLAG_RD, FLAG_TC, HEADER_BYTES, Header, Message,
        Question, Record, Section, TYPE_OPT,
    },
};

const DNSSEC_OK: u32 = 1 << 15;
const TYPE_TSIG: u16 = 250;
const TYPE_SIG: u16 = 24;
const CLASS_IN: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Profile {
    kind: u16,
    class: u16,
    flags: u16,
    edns_udp_size: Option<u16>,
    dnssec: bool,
}

/// One ordinary DNS question and its downstream response constraints.
#[derive(Clone, Copy)]
pub struct Query<'a> {
    message: Message<'a>,
    profile: Option<Profile>,
    edns: Option<(u16, usize)>,
}

impl<'a> Query<'a> {
    /// Verify an unsigned, single-question QUERY and its optional EDNS record.
    /// # Errors
    /// The envelope, section counts or EDNS options are malformed, or this exchange needs a
    /// different opcode, transfer, or message authentication implementation.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let message = Message::parse(bytes)?;
        let header = message.header;
        if header.flags & FLAG_QR != 0 || header.answers != 0 || header.authorities != 0 {
            return Err(PacketError::Malformed);
        }
        if header.opcode() != 0 || matches!(message.question.qtype, 249..=254) {
            return Err(PacketError::Unsupported);
        }
        if header.flags & !(FLAG_RD | FLAG_AD | FLAG_CD) != 0 {
            return Err(PacketError::Malformed);
        }
        let mut profile = (message.question.qclass == CLASS_IN).then_some(Profile {
            kind: message.question.qtype,
            class: message.question.qclass,
            flags: header.flags,
            edns_udp_size: None,
            dnssec: false,
        });
        let mut advertised = None;
        for record in message.records() {
            let record = record?;
            if record.kind != TYPE_OPT {
                return Err(PacketError::Unsupported);
            }
            if advertised.is_some() || !record.root_owner || record.section != Section::Additional {
                return Err(PacketError::Malformed);
            }
            validate_options(record.data)?;
            advertised = Some((record.class.max(512), record.ttl_offset - 2));
            if record.ttl & !DNSSEC_OK != 0 || !record.data.is_empty() {
                profile = None;
            }
            if let Some(profile) = &mut profile {
                profile.edns_udp_size = advertised.map(|(size, _)| size);
                profile.dnssec = record.ttl & DNSSEC_OK != 0;
            }
        }
        Ok(Self {
            message,
            profile,
            edns: advertised,
        })
    }

    /// Message as received, including the client's transaction ID.
    pub fn bytes(self) -> &'a [u8] {
        self.message.bytes()
    }

    /// Original DNS header.
    pub fn header(self) -> Header {
        self.message.header
    }

    /// Largest UDP payload the downstream client advertised.
    pub fn udp_size(self) -> usize {
        usize::from(self.edns.map_or(512, |(size, _)| size))
    }

    /// Location of the optional EDNS UDP payload size in the original query.
    pub fn udp_size_offset(self) -> Option<usize> {
        self.edns.map(|(_, offset)| offset)
    }

    /// Match an upstream response to the rewritten transaction ID and original question.
    /// The adapter must also validate the socket and endpoint that received the response.
    /// # Errors
    /// The upstream header or question is malformed or does not match the request.
    pub fn match_reply(self, upstream_id: u16, bytes: &[u8]) -> Result<Header, PacketError> {
        let (header, question) = Message::read_question(bytes)?;
        let Question {
            name,
            qtype,
            qclass,
        } = self.message.question;
        if header.flags & FLAG_QR == 0
            || header.id != upstream_id
            || header.opcode() != self.header().opcode()
            || !question.name.bytes().eq_ignore_ascii_case(name.bytes())
            || question.qtype != qtype
            || question.qclass != qclass
        {
            return Err(PacketError::Malformed);
        }
        Ok(header)
    }
    /// Validate a matched, complete upstream answer before sending or storing it.
    /// # Errors
    /// The transaction or question does not match, the message is truncated, or its envelope,
    /// extension records or message authentication do not permit relaying with a different ID.
    pub fn verify_reply<'r>(
        self,
        upstream_id: u16,
        bytes: &'r [u8],
    ) -> Result<Answer<'a, 'r>, PacketError> {
        if self.match_reply(upstream_id, bytes)?.flags & FLAG_TC != 0 {
            return Err(PacketError::Truncated);
        }
        let message = Message::parse(bytes)?;
        let mut edns_ttl = None;
        for record in message.records() {
            let record = record?;
            if record.kind == TYPE_OPT {
                if edns_ttl.is_some()
                    || self.edns.is_none()
                    || record.section != Section::Additional
                    || !record.root_owner
                {
                    return Err(PacketError::Malformed);
                }
                validate_options(record.data)?;
                edns_ttl = Some(record.ttl);
            } else if authenticated(&record) {
                return Err(PacketError::Unsupported);
            }
        }
        Ok(Answer {
            query: self,
            message,
            edns_ttl,
        })
    }

    fn needs_tcp(self, length: usize, udp_limit: Option<usize>) -> bool {
        udp_limit.is_some_and(|limit| length > limit.min(self.udp_size()))
    }

    /// Encode SERVFAIL after exhausting the configured upstream attempts.
    /// # Errors
    /// The destination cannot hold the question and required EDNS response.
    pub fn failure(self, output: &mut [u8]) -> Result<usize, PacketError> {
        let mut header = self.header();
        header.flags = FLAG_QR | FLAG_RA | (header.flags & (FLAG_RD | FLAG_CD)) | 2;
        self.question_reply(header, 512, None, output)
    }

    fn truncated(
        self,
        mut header: Header,
        udp_limit: usize,
        edns_ttl: Option<u32>,
        output: &mut [u8],
    ) -> Result<usize, PacketError> {
        header.flags = (header.flags | FLAG_TC) & !FLAG_AD;
        self.question_reply(header, udp_limit, edns_ttl, output)
    }

    fn question_reply(
        self,
        mut header: Header,
        udp_limit: usize,
        edns_ttl: Option<u32>,
        output: &mut [u8],
    ) -> Result<usize, PacketError> {
        let question_end = HEADER_BYTES + self.message.question.length();
        let mut length = question_end;
        header.id = self.header().id;
        header.answers = 0;
        header.authorities = 0;
        header.additional = u16::from(self.edns.is_some());
        if self.edns.is_some() {
            length += 11;
        }
        if length > udp_limit {
            return Err(PacketError::Capacity);
        }
        let output = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        header.emit(output)?;
        output[HEADER_BYTES..question_end]
            .copy_from_slice(&self.bytes()[HEADER_BYTES..question_end]);
        if let Some(opt) = output
            .get_mut(question_end..length)
            .filter(|opt| !opt.is_empty())
        {
            opt.fill(0);
            opt[1..3].copy_from_slice(&TYPE_OPT.to_be_bytes());
            let size = u16::try_from(udp_limit).unwrap_or(u16::MAX).max(512);
            opt[3..5].copy_from_slice(&size.to_be_bytes());
            opt[5..9].copy_from_slice(&edns_ttl.unwrap_or(0).to_be_bytes());
        }
        Ok(length)
    }
}

/// Complete matched answer, safe to relay or evaluate for message caching.
#[derive(Clone, Copy)]
pub struct Answer<'q, 'r> {
    query: Query<'q>,
    message: Message<'r>,
    edns_ttl: Option<u32>,
}

impl Answer<'_, '_> {
    /// Encode the reply with the downstream transaction ID. If `udp_limit` is present, a reply
    /// that exceeds that ceiling or the client's advertised size becomes a truncated response
    /// that asks the client to retry over TCP.
    /// # Errors
    /// The caller's output cannot contain the complete reply or its truncated question.
    pub fn emit(self, udp_limit: Option<usize>, output: &mut [u8]) -> Result<usize, PacketError> {
        let bytes = self.message.bytes();
        if let Some(limit) =
            udp_limit.filter(|&limit| self.query.needs_tcp(bytes.len(), Some(limit)))
        {
            return self
                .query
                .truncated(self.message.header, limit, self.edns_ttl, output);
        }
        let output = output.get_mut(..bytes.len()).ok_or(PacketError::Capacity)?;
        output.copy_from_slice(bytes);
        output[..2].copy_from_slice(&self.query.header().id.to_be_bytes());
        let question_end = HEADER_BYTES + self.query.message.question.length();
        output[HEADER_BYTES..question_end]
            .copy_from_slice(&self.query.bytes()[HEADER_BYTES..question_end]);
        Ok(bytes.len())
    }
}

fn authenticated(record: &Record<'_>) -> bool {
    record.kind == TYPE_TSIG
        || (record.kind == TYPE_SIG
            && record.section == Section::Additional
            && record.data.starts_with(&[0, 0]))
}

fn validate_options(mut bytes: &[u8]) -> Result<(), PacketError> {
    while !bytes.is_empty() {
        let (header, data) = bytes
            .split_first_chunk::<4>()
            .ok_or(PacketError::Truncated)?;
        let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        bytes = data.get(length..).ok_or(PacketError::Truncated)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
