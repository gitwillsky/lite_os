use alloc::vec::Vec;
use crate::{open, read, close};
use crate::syscall::open_flags;

pub fn read_all(path: &str) -> Option<Vec<u8>> {
	let fd = open(path, open_flags::O_RDONLY) as i32;
	if fd < 0 {
		println!("[webcore::loader] open failed: {}", path);
		return None;
	}
	let mut out: Vec<u8> = Vec::new();
	loop {
		let mut chunk = alloc::vec![0u8; 64 * 1024];
		let n = read(fd as usize, &mut chunk);
		if n > 0 {
			let nn = n as usize;
			chunk.truncate(nn);
			out.extend_from_slice(&chunk);
			if nn < 64 * 1024 { break; }
		} else { break; }
	}
	let _ = close(fd as usize);
	if out.is_empty() {
		println!("[webcore::loader] read empty: {}", path);
		None
	} else {
		println!("[webcore::loader] read {} bytes from {}", out.len(), path);
		Some(out)
	}
}
