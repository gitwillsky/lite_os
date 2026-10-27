//! DRM fdinfo 的实际 GEM 生命周期回归，使用同一标准 ioctl 与 procfs 路径。
use linux_uapi::drm::DrmDevice;
use std::{
    fs, io,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
};

/// 返回（client-id, resident KiB, shared KiB）；共享量不得超过同一客户端的总量。
pub(super) fn read(fd: BorrowedFd<'_>) -> io::Result<(u64, u64, u64)> {
    let text = fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))?;
    let field = |key| {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name == key).then_some(value.trim())
        })
    };
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing or inconsistent DRM fdinfo",
        )
    };
    if field("drm-driver") != Some("virtio_gpu") {
        return Err(invalid());
    }
    let client = field("drm-client-id")
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid)?;
    let kib = |key| {
        field(key)
            .and_then(|value| value.strip_suffix(" KiB"))
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(invalid)
    };
    let memory = kib("drm-resident-memory")?;
    let shared = kib("drm-shared-memory")?;
    if kib("drm-total-memory")? != memory || shared > memory {
        return Err(invalid());
    }
    Ok((client, memory, shared))
}

pub(super) fn verify() -> io::Result<()> {
    let device = DrmDevice::open("/dev/dri/card0")?;
    let other = DrmDevice::open("/dev/dri/card0")?;
    let duplicate = device.as_fd().try_clone_to_owned()?;
    let (client, initial, _) = read(device.as_fd())?;
    if initial != 0
        || read(other.as_fd())?.0 == client
        || read(duplicate.as_fd())? != (client, 0, 0)
    {
        return Err(io::Error::other(
            "DRM OFD identity is not stable across dup or distinct across open",
        ));
    }
    let buffer = device.create_dumb(65, 17)?;
    let allocated = buffer.size() as u64 / 1024;
    if read(duplicate.as_fd())? != (client, allocated, 0) || read(other.as_fd())?.1 != 0 {
        return Err(io::Error::other(
            "DRM fdinfo allocation accounting crossed OFD boundaries",
        ));
    }
    drop(buffer);
    if read(device.as_fd())? != (client, 0, 0) {
        return Err(io::Error::other("DRM fdinfo retained a closed GEM handle"));
    }
    println!("drm-flip-probe: fdinfo=ok identity=ok dup=ok gem-close=ok");
    Ok(())
}
