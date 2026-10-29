//! UAC2 Type-I PCM descriptor、clock topology 与实际 rate 集合；不猜测 VID/PID 的音频能力。
pub(super) mod stream;
use super::{Error, Result};
use crate::devices::model::{PCM_RATES, PcmCapabilities, PcmFormat};
use alloc::vec::Vec;
pub(super) use stream::Stream;

#[derive(Clone, Copy)]
pub(super) struct Endpoint {
    pub(in crate::drivers::usb) address: u8,
    pub(in crate::drivers::usb) packet: u16,
    pub(in crate::drivers::usb) interval: u8,
}
impl Endpoint {
    pub(in crate::drivers::usb) fn dci(self) -> u8 {
        (self.address & 15) * 2 + u8::from(self.address & 0x80 != 0)
    }
    pub(in crate::drivers::usb) fn payload(self) -> usize {
        usize::from(self.packet & 0x7ff) * (usize::from(self.packet >> 11 & 3) + 1)
    }
}
#[derive(Clone, Copy)]
pub(super) struct Alternate {
    pub(in crate::drivers::usb) interface: u8,
    pub(in crate::drivers::usb) alternate: u8,
    pub(in crate::drivers::usb) terminal: u8,
    pub(in crate::drivers::usb) format: PcmFormat,
    pub(in crate::drivers::usb) output: Endpoint,
    pub(in crate::drivers::usb) feedback: Option<Endpoint>,
    pub(in crate::drivers::usb) rates: u32,
    pub(in crate::drivers::usb) clock: u8,
    pub(in crate::drivers::usb) clock_controls: u8,
}
pub(super) struct Configuration {
    pub(in crate::drivers::usb) value: u8,
    pub(in crate::drivers::usb) control: u8,
    pub(in crate::drivers::usb) alternates: Vec<Alternate>,
}
/// 有界 descriptor 游标；损坏长度返回错误，不能越过下一个 interface 偷用其 endpoint/clock。
pub(super) fn descriptors(bytes: &[u8]) -> Result<impl Iterator<Item = &[u8]>> {
    let mut offset = 0;
    while offset < bytes.len() {
        let len = usize::from(*bytes.get(offset).ok_or(Error::Protocol)?);
        if len < 2 || offset + len > bytes.len() {
            return Err(Error::Protocol);
        }
        offset += len;
    }
    let mut offset = 0;
    Ok(core::iter::from_fn(move || {
        if offset == bytes.len() {
            return None;
        }
        let len = usize::from(bytes[offset]);
        let part = &bytes[offset..offset + len];
        offset += len;
        Some(part)
    }))
}

pub(super) fn parse(bytes: &[u8]) -> Result<Configuration> {
    if bytes.len() < 9
        || bytes[0] != 9
        || bytes[1] != 2
        || usize::from(u16::from_le_bytes([bytes[2], bytes[3]])) != bytes.len()
    {
        return Err(Error::Protocol);
    }
    let mut control = None;
    let mut result = Configuration {
        value: *bytes.get(5).ok_or(Error::Protocol)?,
        control: 0,
        alternates: Vec::new(),
    };
    let mut interface = None;
    let mut general = None;
    let mut format = None;
    let mut endpoints = [None; 2];
    let mut asynchronous = false;
    fn finish(
        result: &mut Configuration,
        interface: Option<(u8, u8)>,
        general: Option<u8>,
        format: Option<PcmFormat>,
        endpoints: [Option<Endpoint>; 2],
        asynchronous: bool,
    ) -> Result<()> {
        if let (Some((interface, alternate)), Some(terminal), Some(format), Some(output)) =
            (interface, general, format, endpoints[0])
        {
            if asynchronous && endpoints[1].is_none() {
                return Ok(());
            }
            result
                .alternates
                .try_reserve(1)
                .map_err(|_| Error::Memory)?;
            result.alternates.push(Alternate {
                interface,
                alternate,
                terminal,
                format,
                output,
                feedback: endpoints[1],
                rates: 0,
                clock: 0,
                clock_controls: 0,
            });
        }
        Ok(())
    }
    for d in descriptors(bytes)? {
        match d[1] {
            4 if d.len() >= 9 => {
                finish(
                    &mut result,
                    interface,
                    general,
                    format,
                    endpoints,
                    asynchronous,
                )?;
                interface = None;
                general = None;
                format = None;
                endpoints = [None; 2];
                asynchronous = false;
                if d[5..8] == [1, 1, 0x20] {
                    if control.is_some() {
                        return Err(Error::Unsupported);
                    }
                    control = Some(d[2]);
                }
                if d[5..8] == [1, 2, 0x20] && d[3] != 0 {
                    interface = Some((d[2], d[3]));
                }
            }
            0x24 if interface.is_some() && d.len() >= 3 => {
                if d[2] == 1
                    && d.len() >= 16
                    && d[5] == 1
                    && d[10] == 2
                    && u32::from_le_bytes(d[6..10].try_into().map_err(|_| Error::Protocol)?) == 1
                {
                    general = Some(d[3]);
                }
                if d[2] == 2 && d.len() >= 6 && d[3] == 1 {
                    format = match (d[4], d[5]) {
                        (2, 16) => Some(PcmFormat::S16),
                        (3, 24) => Some(PcmFormat::S24Packed),
                        (4, 24) => Some(PcmFormat::S24),
                        (4, 32) => Some(PcmFormat::S32),
                        _ => None,
                    };
                }
            }
            5 if interface.is_some() && d.len() >= 7 && d[3] & 3 == 1 => {
                let endpoint = Endpoint {
                    address: d[2],
                    packet: u16::from_le_bytes([d[4], d[5]]),
                    interval: d[6],
                };
                if endpoint.address & 0x70 != 0 || endpoint.address & 15 == 0 {
                    return Err(Error::Protocol);
                }
                if !(1..=16).contains(&endpoint.interval)
                    || endpoint.payload() > 3072
                    || endpoint.payload() == 0
                {
                    return Err(Error::Unsupported);
                }
                if endpoint.address & 0x80 == 0 && d[3] >> 4 & 3 == 0 {
                    if endpoints[0].is_some() {
                        return Err(Error::Unsupported);
                    }
                    asynchronous = d[3] >> 2 & 3 == 1;
                    if d[3] >> 2 & 3 == 2 {
                        return Err(Error::Unsupported);
                    }
                    endpoints[0] = Some(endpoint);
                } else if endpoint.address & 0x80 != 0 && d[3] >> 4 & 3 == 1 {
                    if endpoint.payload() != 4 {
                        return Err(Error::Unsupported);
                    }
                    endpoints[1] = Some(endpoint);
                }
            }
            _ => {}
        }
    }
    finish(
        &mut result,
        interface,
        general,
        format,
        endpoints,
        asynchronous,
    )?;
    result.control = control.ok_or(Error::Unsupported)?;
    if result.alternates.is_empty() {
        return Err(Error::Unsupported);
    }
    Ok(result)
}
pub(super) fn rate_mask(range: &[u8], alt: Alternate) -> Result<u32> {
    let count = usize::from(u16::from_le_bytes(
        range
            .get(..2)
            .ok_or(Error::Protocol)?
            .try_into()
            .map_err(|_| Error::Protocol)?,
    ));
    if range.len() != 2 + count * 12 {
        return Err(Error::Protocol);
    }
    let mut mask = 0;
    for (index, rate) in PCM_RATES.iter().enumerate() {
        let fits = range[2..].as_chunks::<12>().0.iter().any(|r| {
            let min = u32::from_le_bytes(r[..4].try_into().unwrap());
            let max = u32::from_le_bytes(r[4..8].try_into().unwrap());
            let step = u32::from_le_bytes(r[8..12].try_into().unwrap());
            *rate >= min && *rate <= max && (step == 0 || (rate - min).is_multiple_of(step))
        });
        let frames = u64::from(*rate).div_ceil(8000) * (1u64 << (alt.output.interval - 1));
        if fits && frames * (alt.format.bytes() * 2) as u64 <= alt.output.payload() as u64 {
            mask |= 1 << index;
        }
    }
    Ok(mask)
}
pub(super) fn capabilities(alternates: &[Alternate]) -> PcmCapabilities {
    let mut caps = PcmCapabilities::EMPTY;
    for alt in alternates {
        caps.rates[alt.format as usize] |= alt.rates;
    }
    caps
}
