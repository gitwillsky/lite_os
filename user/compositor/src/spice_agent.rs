//! SPICE guest-agent transport for dynamic display configuration and clipboard state.

use std::{
    collections::VecDeque,
    io,
    os::fd::{AsFd, BorrowedFd},
};

use display_proto::{
    ClipboardData, ClipboardRead, ClipboardWrite, MAX_APP_SURFACES, MAX_CLIPBOARD_TEXT, Size,
};
use linux_uapi::virtio_port::SpicePort;

const VDP_CLIENT_PORT: u32 = 1;
const VD_AGENT_PROTOCOL: u32 = 1;
const VD_AGENT_MONITORS_CONFIG: u32 = 2;
const VD_AGENT_CLIPBOARD: u32 = 4;
const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 6;
const VD_AGENT_CLIPBOARD_GRAB: u32 = 7;
const VD_AGENT_CLIPBOARD_REQUEST: u32 = 8;
const VD_AGENT_CLIPBOARD_RELEASE: u32 = 9;
const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;
const VD_AGENT_CAP_MONITORS_CONFIG: u32 = 1;
const VD_AGENT_CAP_CLIPBOARD_BY_DEMAND: u32 = 5;
const VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS: u32 = 1;
const VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE: u32 = 2;
const MONITOR_CONFIG_HEADER: usize = 8;
const MONITOR_CONFIG_SIZE: usize = 20;
const MONITOR_PHYSICAL_SIZE: usize = 4;
const MAX_OUTPUT_DIMENSION: u32 = 8192;
const CHUNK_PAYLOAD: usize = 1024;
const AGENT_HEADER: usize = 20;
const MAX_AGENT_MESSAGE: usize = 1024 * 1024 + AGENT_HEADER;

enum Owner {
    Empty,
    Local(String),
    Remote(Option<String>),
}

#[derive(Debug)]
enum AgentEvent {
    Capabilities { request: bool },
    Monitor(Size),
    Grab { text: bool },
    RequestText,
    Data(String),
    Release,
}

/// Results produced by one nonblocking SPICE agent drain.
pub(super) struct AgentPump {
    /// Clipboard replies whose pending guest reads can now complete.
    pub(super) clipboard: Vec<ClipboardData>,
    /// Latest single-monitor pixel size requested by the UTM display client.
    pub(super) monitor: Option<Size>,
}

/// The compositor-owned SPICE transport, display request stream and clipboard state.
pub(super) struct SpiceAgent {
    port: SpicePort,
    wire: Vec<u8>,
    message: Vec<u8>,
    expected_message: Option<usize>,
    output: VecDeque<u8>,
    owner: Owner,
    pending: Vec<ClipboardRead>,
    remote_request_outstanding: bool,
}

impl SpiceAgent {
    /// Opens the standard SPICE port and starts capability negotiation.
    pub(super) fn open() -> io::Result<Self> {
        let mut value = Self {
            port: SpicePort::open()?,
            wire: Vec::new(),
            message: Vec::new(),
            expected_message: None,
            output: VecDeque::new(),
            owner: Owner::Empty,
            pending: Vec::new(),
            remote_request_outstanding: false,
        };
        value.send_capabilities(true)?;
        Ok(value)
    }

    pub(super) fn as_fd(&self) -> BorrowedFd<'_> {
        self.port.as_fd()
    }

    pub(super) fn wants_write(&self) -> bool {
        !self.output.is_empty()
    }

    /// Publishes focused guest text and advertises ownership to macOS.
    pub(super) fn write(&mut self, value: ClipboardWrite) -> io::Result<()> {
        self.owner = Owner::Local(value.text);
        self.pending.clear();
        self.remote_request_outstanding = false;
        self.send_agent(
            VD_AGENT_CLIPBOARD_GRAB,
            &VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes(),
        )
    }

    /// Resolves immediately from cached ownership or requests lazy host data.
    pub(super) fn read(&mut self, request: ClipboardRead) -> io::Result<Option<ClipboardData>> {
        match &self.owner {
            Owner::Empty => Ok(Some(Self::data(request, String::new()))),
            Owner::Local(text) | Owner::Remote(Some(text)) => {
                Ok(Some(Self::data(request, text.clone())))
            }
            Owner::Remote(None) => {
                if self.pending.len() > MAX_APP_SURFACES {
                    return Err(io::Error::other("clipboard request capacity exhausted"));
                }
                self.pending.push(request);
                if !self.remote_request_outstanding {
                    self.send_agent(
                        VD_AGENT_CLIPBOARD_REQUEST,
                        &VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes(),
                    )?;
                    self.remote_request_outstanding = true;
                }
                Ok(None)
            }
        }
    }

    pub(super) fn remove_surface(&mut self, surface_id: u32) {
        self.pending
            .retain(|request| request.surface_id != surface_id);
    }

    pub(super) fn reset_session(&mut self) {
        self.pending.clear();
    }

    /// Drains nonblocking port I/O and coalesces standard agent events.
    pub(super) fn pump(&mut self) -> io::Result<AgentPump> {
        self.flush()?;
        let mut bytes = [0u8; 4096];
        loop {
            match self.port.read(&mut bytes) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "vdagent EOF")),
                Ok(length) => self.wire.extend_from_slice(&bytes[..length]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        let events = self.parse_wire()?;
        let mut replies = Vec::new();
        let mut monitor = None;
        for event in events {
            match event {
                AgentEvent::Capabilities { request } => {
                    if request {
                        self.send_capabilities(false)?;
                    }
                }
                AgentEvent::Monitor(size) => monitor = Some(size),
                AgentEvent::Grab { text } => {
                    self.pending.clear();
                    self.remote_request_outstanding = false;
                    self.owner = if text {
                        Owner::Remote(None)
                    } else {
                        Owner::Empty
                    };
                }
                AgentEvent::RequestText => {
                    let text = match &self.owner {
                        Owner::Local(text) => text.as_str(),
                        _ => "",
                    };
                    let mut data = Vec::with_capacity(4 + text.len());
                    data.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());
                    data.extend_from_slice(text.as_bytes());
                    self.send_agent(VD_AGENT_CLIPBOARD, &data)?;
                }
                AgentEvent::Data(text) => {
                    if matches!(self.owner, Owner::Remote(_)) && self.remote_request_outstanding {
                        self.owner = Owner::Remote(Some(text.clone()));
                        self.remote_request_outstanding = false;
                        replies.reserve(self.pending.len());
                        for request in self.pending.drain(..) {
                            replies.push(Self::data(request, text.clone()));
                        }
                    }
                }
                AgentEvent::Release => {
                    if matches!(self.owner, Owner::Remote(_)) {
                        self.owner = Owner::Empty;
                        self.remote_request_outstanding = false;
                        for request in self.pending.drain(..) {
                            replies.push(Self::data(request, String::new()));
                        }
                    }
                }
            }
        }
        self.flush()?;
        Ok(AgentPump {
            clipboard: replies,
            monitor,
        })
    }

    fn data(request: ClipboardRead, text: String) -> ClipboardData {
        ClipboardData {
            surface_id: request.surface_id,
            request_id: request.request_id,
            text,
        }
    }

    fn send_capabilities(&mut self, request: bool) -> io::Result<()> {
        let mut data = [0u8; 8];
        data[..4].copy_from_slice(&u32::from(request).to_le_bytes());
        let capabilities =
            1u32 << VD_AGENT_CAP_MONITORS_CONFIG | 1u32 << VD_AGENT_CAP_CLIPBOARD_BY_DEMAND;
        data[4..].copy_from_slice(&capabilities.to_le_bytes());
        self.send_agent(VD_AGENT_ANNOUNCE_CAPABILITIES, &data)
    }

    fn send_agent(&mut self, kind: u32, data: &[u8]) -> io::Result<()> {
        let length = AGENT_HEADER
            .checked_add(data.len())
            .filter(|length| *length <= MAX_AGENT_MESSAGE)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "vdagent message too large")
            })?;
        let mut message = Vec::with_capacity(length);
        message.extend_from_slice(&VD_AGENT_PROTOCOL.to_le_bytes());
        message.extend_from_slice(&kind.to_le_bytes());
        message.extend_from_slice(&0u64.to_le_bytes());
        message.extend_from_slice(&(data.len() as u32).to_le_bytes());
        message.extend_from_slice(data);
        for fragment in message.chunks(CHUNK_PAYLOAD) {
            self.output.extend(VDP_CLIENT_PORT.to_le_bytes());
            self.output.extend((fragment.len() as u32).to_le_bytes());
            self.output.extend(fragment);
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        while !self.output.is_empty() {
            let (first, _) = self.output.as_slices();
            match self.port.write(first) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "vdagent write zero",
                    ));
                }
                Ok(length) => {
                    self.output.drain(..length);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn parse_wire(&mut self) -> io::Result<Vec<AgentEvent>> {
        let mut events = Vec::new();
        let mut consumed = 0usize;
        while self.wire.len() - consumed >= 8 {
            let port = read_u32(&self.wire, consumed)?;
            let size = read_u32(&self.wire, consumed + 4)? as usize;
            if port != VDP_CLIENT_PORT || size == 0 || size > CHUNK_PAYLOAD {
                return Err(invalid("invalid vdagent chunk"));
            }
            if self.wire.len() - consumed < 8 + size {
                break;
            }
            self.message
                .extend_from_slice(&self.wire[consumed + 8..consumed + 8 + size]);
            consumed += 8 + size;
            if self.expected_message.is_none() && self.message.len() >= AGENT_HEADER {
                if read_u32(&self.message, 0)? != VD_AGENT_PROTOCOL {
                    return Err(invalid("invalid vdagent protocol"));
                }
                let payload = read_u32(&self.message, 16)? as usize;
                self.expected_message = Some(
                    AGENT_HEADER
                        .checked_add(payload)
                        .filter(|length| *length <= MAX_AGENT_MESSAGE)
                        .ok_or_else(|| invalid("vdagent payload too large"))?,
                );
            }
            if let Some(expected) = self.expected_message {
                if self.message.len() > expected {
                    return Err(invalid("vdagent chunk crossed message boundary"));
                }
                if self.message.len() == expected {
                    if let Some(event) = parse_agent(&self.message)? {
                        events.push(event);
                    }
                    self.message.clear();
                    self.expected_message = None;
                }
            }
        }
        if consumed != 0 {
            self.wire.drain(..consumed);
        }
        Ok(events)
    }
}

fn parse_agent(message: &[u8]) -> io::Result<Option<AgentEvent>> {
    if message.len() < AGENT_HEADER || read_u32(message, 0)? != VD_AGENT_PROTOCOL {
        return Err(invalid("invalid vdagent message"));
    }
    let kind = read_u32(message, 4)?;
    let size = read_u32(message, 16)? as usize;
    if AGENT_HEADER + size != message.len() {
        return Err(invalid("invalid vdagent message length"));
    }
    let data = &message[AGENT_HEADER..];
    Ok(match kind {
        VD_AGENT_ANNOUNCE_CAPABILITIES if data.len() >= 8 => Some(AgentEvent::Capabilities {
            request: read_u32(data, 0)? != 0,
        }),
        VD_AGENT_MONITORS_CONFIG => parse_monitor_config(data)?.map(AgentEvent::Monitor),
        VD_AGENT_CLIPBOARD_GRAB if data.len().is_multiple_of(4) => {
            let text = data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|value| read_u32(value, 0).ok() == Some(VD_AGENT_CLIPBOARD_UTF8_TEXT));
            Some(AgentEvent::Grab { text })
        }
        VD_AGENT_CLIPBOARD_REQUEST
            if data.len() == 4 && read_u32(data, 0)? == VD_AGENT_CLIPBOARD_UTF8_TEXT =>
        {
            Some(AgentEvent::RequestText)
        }
        VD_AGENT_CLIPBOARD
            if data.len() >= 4 && read_u32(data, 0)? == VD_AGENT_CLIPBOARD_UTF8_TEXT =>
        {
            let bytes = &data[4..];
            if bytes.len() > MAX_CLIPBOARD_TEXT {
                Some(AgentEvent::Data(String::new()))
            } else {
                Some(AgentEvent::Data(
                    std::str::from_utf8(bytes)
                        .map_err(|_| invalid("vdagent clipboard is not UTF-8"))?
                        .to_owned(),
                ))
            }
        }
        VD_AGENT_CLIPBOARD_RELEASE if data.is_empty() => Some(AgentEvent::Release),
        _ => None,
    })
}

fn parse_monitor_config(data: &[u8]) -> io::Result<Option<Size>> {
    if data.len() < MONITOR_CONFIG_HEADER {
        return Err(invalid("truncated vdagent monitor configuration"));
    }
    let count = usize::try_from(read_u32(data, 0)?)
        .map_err(|_| invalid("invalid vdagent monitor count"))?;
    if count != 1 {
        return Err(invalid("LiteOS requires exactly one SPICE monitor"));
    }
    let flags = read_u32(data, 4)?;
    if flags
        & !(VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS | VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE)
        != 0
    {
        return Err(invalid("unsupported vdagent monitor flags"));
    }
    let expected = MONITOR_CONFIG_HEADER
        + MONITOR_CONFIG_SIZE
        + usize::from(flags & VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE != 0)
            * MONITOR_PHYSICAL_SIZE;
    if data.len() != expected {
        return Err(invalid("invalid vdagent monitor configuration length"));
    }
    let height = read_u32(data, 8)?;
    let requested_width = read_u32(data, 12)?;
    let depth = read_u32(data, 16)?;
    let x = read_u32(data, 20)? as i32;
    let y = read_u32(data, 24)? as i32;
    if x != 0 || y != 0 || !matches!(depth, 0 | 32) {
        return Err(invalid("unsupported vdagent monitor geometry"));
    }
    // Linux's non-reduced CVT helper canonicalizes horizontal pixels to 8-pixel
    // character cells. Applying that standard normalization here prevents a
    // one-to-seven-pixel UTM window increment from producing a non-modeable KMS size.
    let width = requested_width / 8 * 8;
    if width == 0 || height == 0 || width > MAX_OUTPUT_DIMENSION || height > MAX_OUTPUT_DIMENSION {
        return Err(invalid("vdagent monitor dimensions are out of range"));
    }
    Ok(Some(Size { width, height }))
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| invalid("truncated vdagent integer"))?
            .try_into()
            .unwrap(),
    ))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: u32, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(AGENT_HEADER + data.len());
        bytes.extend_from_slice(&VD_AGENT_PROTOCOL.to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[test]
    fn parses_demand_capability_and_text_grab() {
        let mut capabilities = Vec::new();
        capabilities.extend_from_slice(&1u32.to_le_bytes());
        capabilities.extend_from_slice(&(1u32 << VD_AGENT_CAP_CLIPBOARD_BY_DEMAND).to_le_bytes());
        assert!(matches!(
            parse_agent(&message(VD_AGENT_ANNOUNCE_CAPABILITIES, &capabilities))
                .expect("capability message"),
            Some(AgentEvent::Capabilities { request: true })
        ));
        assert!(matches!(
            parse_agent(&message(
                VD_AGENT_CLIPBOARD_GRAB,
                &VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes()
            ))
            .expect("grab message"),
            Some(AgentEvent::Grab { text: true })
        ));
    }

    #[test]
    fn parses_single_monitor_and_applies_cvt_granularity() {
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS.to_le_bytes());
        data.extend_from_slice(&1179u32.to_le_bytes());
        data.extend_from_slice(&2047u32.to_le_bytes());
        data.extend_from_slice(&32u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            parse_agent(&message(VD_AGENT_MONITORS_CONFIG, &data)).expect("monitor message"),
            Some(AgentEvent::Monitor(Size {
                width: 2040,
                height: 1179
            }))
        ));
    }

    #[test]
    fn monitor_config_rejects_topologies_the_drm_owner_cannot_represent() {
        let mut data = Vec::new();
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.resize(MONITOR_CONFIG_HEADER + 2 * MONITOR_CONFIG_SIZE, 0);
        assert_eq!(
            parse_agent(&message(VD_AGENT_MONITORS_CONFIG, &data))
                .expect_err("multiple monitors must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn clipboard_data_requires_utf8_and_enforces_display_limit() {
        let mut invalid_text = Vec::from(VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());
        invalid_text.push(0xff);
        assert!(
            parse_agent(&message(VD_AGENT_CLIPBOARD, &invalid_text))
                .expect_err("invalid UTF-8 must fail")
                .kind()
                == io::ErrorKind::InvalidData
        );

        let mut oversized = Vec::from(VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());
        oversized.resize(4 + MAX_CLIPBOARD_TEXT + 1, b'x');
        assert!(matches!(
            parse_agent(&message(VD_AGENT_CLIPBOARD, &oversized))
                .expect("oversized host data is a valid agent message"),
            Some(AgentEvent::Data(value)) if value.is_empty()
        ));
    }

    #[test]
    fn rejects_message_length_mismatch() {
        let mut bytes = message(VD_AGENT_CLIPBOARD_RELEASE, &[]);
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            parse_agent(&bytes)
                .expect_err("mismatched payload length must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
