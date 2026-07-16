use display_client::{Device, Seat};

use crate::ffi;

use super::decimal;

pub(super) fn open_matching(seat: &mut Seat, needles: &[&[u8]]) -> Result<Option<Device>, ()> {
    for index in 0..16u32 {
        let mut path = [0u8; 32];
        let prefix = b"/dev/input/event";
        path[..prefix.len()].copy_from_slice(prefix);
        let capacity = path.len() - 1;
        let length = prefix.len() + decimal(index, &mut path[prefix.len()..capacity]);
        path[length] = 0;
        let Ok(device) = seat.open_device(path.as_ptr().cast()) else {
            continue;
        };
        let mut name = [0u8; 128];
        let named =
            unsafe { ffi::ioctl(device.fd, ffi::EVIOCGNAME_128, name.as_mut_ptr().cast()) } >= 0;
        if named && needles.iter().any(|needle| contains(&name, needle)) {
            let mut grab = 1i32;
            unsafe { ffi::ioctl(device.fd, ffi::EVIOCGRAB, (&mut grab as *mut i32).cast()) };
            return Ok(Some(device));
        }
        seat.close_device(device)?;
    }
    Ok(None)
}

fn contains(name: &[u8], needle: &[u8]) -> bool {
    let mut matched = 0;
    for byte in name.iter().copied().take_while(|byte| *byte != 0) {
        let value = byte.to_ascii_lowercase();
        matched = if value == needle[matched] {
            matched + 1
        } else {
            usize::from(value == needle[0])
        };
        if matched == needle.len() {
            return true;
        }
    }
    false
}
