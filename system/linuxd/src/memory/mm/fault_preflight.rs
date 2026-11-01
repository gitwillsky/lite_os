/// User access which caused a page fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultAccess {
    Read,
    Write,
    Execute,
}

/// Access bits owned by the matched VMA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FaultPermissions {
    user: bool,
    read: bool,
    write: bool,
    execute: bool,
}

impl FaultPermissions {
    pub(super) const fn new(user: bool, read: bool, write: bool, execute: bool) -> Self {
        Self {
            user,
            read,
            write,
            execute,
        }
    }
}

/// File-page state after permission validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileFaultState {
    NotFile,
    Available,
    BeyondEof,
}

/// Residency owner selected by the VMA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FaultResidency {
    Device,
    SharedAnonymous,
    SharedFile,
    Private { lazy: bool, resident: bool },
}

/// 不可恢复 user fault 的原因，决定 Linux `SIGSEGV` 的 `si_code`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentationCause {
    /// 没有用户 VMA 覆盖该地址（`SEGV_MAPERR`）。
    Unmapped,
    /// VMA 存在但不允许本次访问（`SEGV_ACCERR`）。
    AccessDenied,
}

/// Allocation-free decision made before the fault path may reclaim memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FaultPreflight {
    SegmentationFault(SegmentationCause),
    BusError,
    Device,
    SharedAnonymous,
    SharedFile,
    Private,
    NeedsPrivateFrame,
}

pub(super) fn preflight_fault<E>(
    contains: bool,
    permissions: FaultPermissions,
    access: FaultAccess,
    file: impl FnOnce() -> Result<FileFaultState, E>,
    residency: impl FnOnce() -> FaultResidency,
) -> Result<FaultPreflight, E> {
    if !contains {
        return Ok(FaultPreflight::SegmentationFault(
            SegmentationCause::Unmapped,
        ));
    }
    if !permissions.user {
        return Ok(FaultPreflight::SegmentationFault(
            SegmentationCause::AccessDenied,
        ));
    }
    let permitted = match access {
        FaultAccess::Read => permissions.read,
        FaultAccess::Write => permissions.write,
        FaultAccess::Execute => permissions.execute,
    };
    if !permitted {
        return Ok(FaultPreflight::SegmentationFault(
            SegmentationCause::AccessDenied,
        ));
    }
    let file = file()?;
    if matches!(file, FileFaultState::BeyondEof) {
        return Ok(FaultPreflight::BusError);
    }
    Ok(match residency() {
        FaultResidency::Device => FaultPreflight::Device,
        FaultResidency::SharedAnonymous => FaultPreflight::SharedAnonymous,
        FaultResidency::SharedFile => FaultPreflight::SharedFile,
        FaultResidency::Private {
            lazy: true,
            resident: false,
        } => FaultPreflight::NeedsPrivateFrame,
        FaultResidency::Private { .. } => FaultPreflight::Private,
    })
}
