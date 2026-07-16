const ABI: u64 = 1;
const MAX_SHELL_HEAP: u64 = 8 * 1024 * 1024;
const MAX_APPLICATION_HEAP: u64 = 4 * 1024 * 1024;

pub struct Manifest {
    pub heap_limit: usize,
    pub bundle_sha256: [u8; 32],
}

pub fn parse(bytes: &[u8]) -> Result<Manifest, ()> {
    let mut decoder = Decoder { bytes, cursor: 0 };
    if decoder.header(5)? != 7 {
        return Err(());
    }
    let mut seen = 0u8;
    let mut heap = 0;
    let mut role = Role::Application;
    let mut bundle_sha256 = [0u8; 32];
    let keys: [&[u8]; 7] = [
        b"id",
        b"abi",
        b"heap",
        b"role",
        b"entry",
        b"styles",
        b"bundle-sha256",
    ];
    for expected in keys {
        let key = decoder.text()?;
        if key != expected {
            return Err(());
        }
        let bit = match key {
            b"abi" => (decoder.header(0)? == ABI).then_some(1),
            b"entry" => (decoder.text()? == b"app.mjs").then_some(2),
            b"heap" => {
                heap = decoder.header(0)?;
                (heap != 0).then_some(4)
            }
            b"id" => valid_application_id(decoder.text()?).then_some(8),
            b"role" => {
                role = Role::parse(decoder.text()?)?;
                Some(16)
            }
            b"bundle-sha256" => {
                bundle_sha256.copy_from_slice(decoder.bytes(32)?);
                Some(32)
            }
            b"styles" => (decoder.text()? == b"styles.bin").then_some(64),
            _ => None,
        }
        .ok_or(())?;
        if seen & bit != 0 {
            return Err(());
        }
        seen |= bit;
    }
    let heap_limit = match role {
        Role::SystemShell => MAX_SHELL_HEAP,
        Role::Application => MAX_APPLICATION_HEAP,
    };
    if seen != 0x7f || decoder.cursor != bytes.len() || heap > heap_limit {
        return Err(());
    }
    Ok(Manifest {
        heap_limit: usize::try_from(heap).map_err(|_| ())?,
        bundle_sha256,
    })
}

#[derive(Clone, Copy)]
enum Role {
    SystemShell,
    Application,
}

impl Role {
    fn parse(value: &[u8]) -> Result<Self, ()> {
        match value {
            b"system-shell" => Ok(Self::SystemShell),
            b"application" => Ok(Self::Application),
            _ => Err(()),
        }
    }
}

fn valid_application_id(value: &[u8]) -> bool {
    value.len() > b"org.liteos.".len()
        && value.len() <= 64
        && value.starts_with(b"org.liteos.")
        && value.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(*byte, b'.' | b'-')
        })
}

struct Decoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Decoder<'a> {
    fn header(&mut self, expected_major: u8) -> Result<u64, ()> {
        let initial = self.take(1)?[0];
        if initial >> 5 != expected_major {
            return Err(());
        }
        match initial & 0x1f {
            value @ 0..=23 => Ok(u64::from(value)),
            24 => {
                let value = u64::from(self.take(1)?[0]);
                (value >= 24).then_some(value).ok_or(())
            }
            25 => {
                let value = u64::from(u16::from_be_bytes(
                    self.take(2)?.try_into().map_err(|_| ())?,
                ));
                (value > u8::MAX.into()).then_some(value).ok_or(())
            }
            26 => {
                let value = u64::from(u32::from_be_bytes(
                    self.take(4)?.try_into().map_err(|_| ())?,
                ));
                (value > u16::MAX.into()).then_some(value).ok_or(())
            }
            27 => {
                let value = u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| ())?);
                (value > u32::MAX.into()).then_some(value).ok_or(())
            }
            _ => Err(()),
        }
    }

    fn text(&mut self) -> Result<&'a [u8], ()> {
        let length = usize::try_from(self.header(3)?).map_err(|_| ())?;
        let value = self.take(length)?;
        core::str::from_utf8(value).map_err(|_| ())?;
        Ok(value)
    }

    fn bytes(&mut self, expected_length: usize) -> Result<&'a [u8], ()> {
        let length = usize::try_from(self.header(2)?).map_err(|_| ())?;
        (length == expected_length).then_some(()).ok_or(())?;
        self.take(length)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ()> {
        let end = self.cursor.checked_add(length).ok_or(())?;
        let value = self.bytes.get(self.cursor..end).ok_or(())?;
        self.cursor = end;
        Ok(value)
    }
}
