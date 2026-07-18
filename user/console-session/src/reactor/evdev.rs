use crate::ffi;

use super::decimal;

pub(super) fn open_matching(needles: &[&[u8]]) -> i32 {
    for index in 0..16u32 {
        let mut path = [0u8; 32];
        let prefix = b"/dev/input/event";
        path[..prefix.len()].copy_from_slice(prefix);
        let capacity = path.len() - 1;
        let length = prefix.len() + decimal(index, &mut path[prefix.len()..capacity]);
        path[length] = 0;
        let fd = unsafe {
            ffi::open(
                path.as_ptr().cast(),
                ffi::O_RDONLY | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
            )
        };
        if fd < 0 {
            continue;
        }
        let mut name = [0u8; 128];
        let named = unsafe { ffi::ioctl(fd, ffi::EVIOCGNAME_128, name.as_mut_ptr().cast()) } >= 0;
        if named && needles.iter().any(|needle| contains(&name, needle)) {
            let mut grab = 1i32;
            unsafe { ffi::ioctl(fd, ffi::EVIOCGRAB, (&mut grab as *mut i32).cast()) };
            return fd;
        }
        unsafe { ffi::close(fd) };
    }
    -1
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
