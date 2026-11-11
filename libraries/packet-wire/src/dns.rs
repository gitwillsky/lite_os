//! RFC 1035 DNS message framing and resource-record views, independent of transport and caching.

use crate::{PacketError, dns_name::UncompressedName};

pub const HEADER_BYTES: usize = 12;
pub const TYPE_SOA: u16 = 6;
pub const TYPE_OPT: u16 = 41;
pub const FLAG_QR: u16 = 0x8000;
pub const FLAG_TC: u16 = 0x0200;
pub const FLAG_RD: u16 = 0x0100;
pub const FLAG_RA: u16 = 0x0080;
pub const FLAG_AD: u16 = 0x0020;
pub const FLAG_CD: u16 = 0x0010;

/// Counts and control flags from a DNS message header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub id: u16,
    pub flags: u16,
    pub questions: u16,
    pub answers: u16,
    pub authorities: u16,
    pub additional: u16,
}

impl Header {
    /// Read the fixed DNS header from `bytes`.
    /// # Errors
    /// The header is truncated.
    pub fn parse(bytes: &[u8]) -> Result<Self, PacketError> {
        let (header, _) = bytes
            .split_first_chunk::<HEADER_BYTES>()
            .ok_or(PacketError::Truncated)?;
        let [id, flags, questions, answers, authorities, additional] = header
            .as_chunks::<2>()
            .0
            .try_into()
            .expect("fixed DNS header");
        Ok(Self {
            id: u16::from_be_bytes(id),
            flags: u16::from_be_bytes(flags),
            questions: u16::from_be_bytes(questions),
            answers: u16::from_be_bytes(answers),
            authorities: u16::from_be_bytes(authorities),
            additional: u16::from_be_bytes(additional),
        })
    }

    /// Write the fixed header, leaving the following sections untouched.
    /// # Errors
    /// The destination has no room for the header.
    pub fn emit(self, bytes: &mut [u8]) -> Result<(), PacketError> {
        let (header, _) = bytes
            .split_first_chunk_mut::<HEADER_BYTES>()
            .ok_or(PacketError::Capacity)?;
        let words = [
            self.id,
            self.flags,
            self.questions,
            self.answers,
            self.authorities,
            self.additional,
        ];
        for (output, word) in header.as_chunks_mut::<2>().0.iter_mut().zip(words) {
            *output = word.to_be_bytes();
        }
        Ok(())
    }

    /// The base 4-bit response code. EDNS extends it using the OPT pseudo-record.
    pub fn rcode(self) -> u8 {
        (self.flags & 15) as u8
    }

    /// The operation code shared by queries and their responses.
    pub fn opcode(self) -> u8 {
        ((self.flags >> 11) & 15) as u8
    }
}

/// The first question in an ordinary one-question DNS exchange.
#[derive(Clone, Copy)]
pub struct Question<'a> {
    pub name: UncompressedName<'a>,
    pub qtype: u16,
    pub qclass: u16,
}

impl<'a> Question<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (name, rest) = UncompressedName::parse_prefix(bytes)?;
        let (fields, _) = rest
            .split_first_chunk::<4>()
            .ok_or(PacketError::Truncated)?;
        let [kind, class] = fields
            .as_chunks::<2>()
            .0
            .try_into()
            .expect("fixed DNS question");
        Ok(Self {
            name,
            qtype: u16::from_be_bytes(kind),
            qclass: u16::from_be_bytes(class),
        })
    }

    /// Encoded question size, including QTYPE and QCLASS.
    pub fn length(self) -> usize {
        self.name.bytes().len() + 4
    }
}

/// A DNS message with one initial question and validated outer record boundaries.
#[derive(Clone, Copy)]
pub struct Message<'a> {
    bytes: &'a [u8],
    pub header: Header,
    pub question: Question<'a>,
}

impl<'a> Message<'a> {
    /// Read the question without requiring the rest of the message to be complete.
    /// Useful for matching an upstream truncated response before retrying it over TCP.
    /// # Errors
    /// Header or question is truncated or the message has a different question count.
    pub fn read_question(bytes: &'a [u8]) -> Result<(Header, Question<'a>), PacketError> {
        if bytes.len() > usize::from(u16::MAX) {
            return Err(PacketError::Malformed);
        }
        let header = Header::parse(bytes)?;
        if header.questions != 1 {
            return Err(PacketError::Unsupported);
        }
        Ok((header, Question::parse(&bytes[HEADER_BYTES..])?))
    }

    /// Verify the question, resource-record envelopes and exact section lengths.
    /// Record-specific RDATA remains opaque until interpreted by its caller.
    /// # Errors
    /// The question count is unsupported, a record is malformed, or bytes remain after the sections.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (header, question) = Self::read_question(bytes)?;
        let message = Self {
            bytes,
            header,
            question,
        };
        let mut records = message.records();
        for record in records.by_ref() {
            record?;
        }
        if records.offset != bytes.len() {
            return Err(PacketError::Malformed);
        }
        Ok(message)
    }

    /// Complete message bytes, including the original question case and compression offsets.
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// Iterate resource records in message order, with their section and offsets.
    pub fn records(self) -> Records<'a> {
        Records {
            bytes: self.bytes,
            offset: HEADER_BYTES + self.question.length(),
            answers: self.header.answers,
            authorities: self.header.authorities,
            additional: self.header.additional,
        }
    }
}

/// Sections following the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Answer,
    Authority,
    Additional,
}

/// A validated record envelope with offsets relative to the whole message.
pub struct Record<'a> {
    pub section: Section,
    pub kind: u16,
    pub class: u16,
    pub ttl: u32,
    pub ttl_offset: usize,
    pub root_owner: bool,
    pub data: &'a [u8],
    message: &'a [u8],
    data_offset: usize,
}

impl Record<'_> {
    /// Read the SOA MINIMUM field after the two domain names and other numeric fields.
    /// # Errors
    /// Wrong record type, malformed names, or incorrect numeric field length.
    pub fn soa_minimum(&self) -> Result<u32, PacketError> {
        if self.kind != TYPE_SOA {
            return Err(PacketError::Unsupported);
        }
        let limit = self.data_offset + self.data.len();
        let mailbox = name_end(self.message, self.data_offset, limit)?;
        let numbers = name_end(self.message, mailbox, limit)?;
        let values: &[u8; 20] = self.message[numbers..limit]
            .try_into()
            .map_err(|_| PacketError::Malformed)?;
        Ok(u32::from_be_bytes(
            values[16..].try_into().expect("fixed SOA field"),
        ))
    }
}

/// Resource-record envelopes, yielded in section order.
pub struct Records<'a> {
    bytes: &'a [u8],
    offset: usize,
    answers: u16,
    authorities: u16,
    additional: u16,
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<Record<'a>, PacketError>;

    fn next(&mut self) -> Option<Self::Item> {
        let section = if self.answers != 0 {
            self.answers -= 1;
            Section::Answer
        } else if self.authorities != 0 {
            self.authorities -= 1;
            Section::Authority
        } else if self.additional != 0 {
            self.additional -= 1;
            Section::Additional
        } else {
            return None;
        };
        let record = self.read_record(section);
        if record.is_err() {
            self.answers = 0;
            self.authorities = 0;
            self.additional = 0;
        }
        Some(record)
    }
}

impl<'a> Records<'a> {
    fn read_record(&mut self, section: Section) -> Result<Record<'a>, PacketError> {
        let fields = name_end(self.bytes, self.offset, self.bytes.len())?;
        let root_owner = self.bytes[self.offset] == 0;
        let (header, rest) = self.bytes[fields..]
            .split_first_chunk::<10>()
            .ok_or(PacketError::Truncated)?;
        let [kind, class, ttl_high, ttl_low, length] = header
            .as_chunks::<2>()
            .0
            .try_into()
            .expect("fixed DNS RR envelope");
        let data = rest
            .get(..usize::from(u16::from_be_bytes(length)))
            .ok_or(PacketError::Truncated)?;
        let data_offset = fields + 10;
        self.offset = data_offset + data.len();
        Ok(Record {
            section,
            kind: u16::from_be_bytes(kind),
            class: u16::from_be_bytes(class),
            ttl: (u32::from(u16::from_be_bytes(ttl_high)) << 16)
                | u32::from(u16::from_be_bytes(ttl_low)),
            ttl_offset: fields + 4,
            root_owner,
            data,
            message: self.bytes,
            data_offset,
        })
    }
}

// The original range constrains literal bytes on the wire, including both bytes of a pointer.
// Following a pointer lowers the bound to that pointer's start. DNS compression only refers to
// prior occurrences, so this also prevents a label from reaching back into the chain that read it.
fn name_end(bytes: &[u8], mut offset: usize, mut bound: usize) -> Result<usize, PacketError> {
    let mut consumed = None;
    let mut expanded = 0;
    let mut pointers = 0;
    loop {
        let tag = *bytes
            .get(..bound)
            .and_then(|bytes| bytes.get(offset))
            .ok_or(PacketError::Truncated)?;
        match tag {
            0 => return Ok(consumed.unwrap_or(offset + 1)),
            1..=63 => {
                let length = usize::from(tag) + 1;
                expanded += length;
                if expanded >= 255 {
                    return Err(PacketError::Malformed);
                }
                offset += length;
            }
            0xc0..=0xff => {
                let low = *bytes
                    .get(..bound)
                    .and_then(|bytes| bytes.get(offset + 1))
                    .ok_or(PacketError::Truncated)?;
                let target = (usize::from(tag & 0x3f) << 8) | usize::from(low);
                pointers += 1;
                if target < HEADER_BYTES || target >= offset || pointers > 128 {
                    return Err(PacketError::Malformed);
                }
                consumed.get_or_insert(offset + 2);
                bound = offset;
                offset = target;
            }
            _ => return Err(PacketError::Malformed),
        }
    }
}
