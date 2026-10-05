use crate::{
    fs::AdvisoryLockKey,
    ipc::PipeDirection,
    memory::{FutexKey, SharedFileId},
};

use super::WAIT_SHARD_COUNT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum WaitIndexKey {
    AdvisoryLock {
        key: AdvisoryLockKey,
        id: u64,
    },
    Console {
        exclusive: bool,
        id: u64,
    },
    Deadline {
        deadline: u64,
        id: u64,
    },
    Futex {
        key: FutexKey,
        id: u64,
    },
    Pipe {
        identity: usize,
        direction: u8,
        exclusive: bool,
        id: u64,
    },
    Task {
        tid: usize,
        id: u64,
    },
}

impl WaitIndexKey {
    pub(super) fn shard(self) -> usize {
        let source = match self {
            Self::AdvisoryLock { key, .. } => {
                let (filesystem, inode) = key.wait_identity();
                mix(filesystem as u64, inode)
            }
            Self::Console { .. } => 0x0043_4f4e_534f_4c45,
            Self::Deadline { id, .. } => mix(0x4445_4144_4c49_4e45, id),
            Self::Futex { key, .. } => futex_source(key),
            Self::Pipe {
                identity,
                direction,
                ..
            } => mix(identity as u64, u64::from(direction)),
            Self::Task { tid, .. } => mix(0x5441_534b, tid as u64),
        };
        source as usize & (WAIT_SHARD_COUNT - 1)
    }
}

fn mix(left: u64, right: u64) -> u64 {
    left.rotate_left(17) ^ right.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

fn shared_file(file: SharedFileId) -> u64 {
    mix(file.filesystem as u64, file.inode)
}

fn futex_source(key: FutexKey) -> u64 {
    match key {
        FutexKey::Private {
            address_space,
            address,
        } => mix(address_space as u64, address as u64),
        FutexKey::SharedAnonymous { backing, offset }
        | FutexKey::SharedDevice { backing, offset } => mix(backing, offset as u64),
        FutexKey::SharedFile { file, offset } => mix(shared_file(file), offset),
    }
}

pub(super) const fn pipe_direction(direction: PipeDirection) -> u8 {
    direction as u8
}
