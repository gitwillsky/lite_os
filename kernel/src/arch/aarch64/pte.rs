use bitflags::bitflags;

bitflags! {
    /// Architecture-neutral mapping permissions consumed by the generic memory owner.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct PagePermissions: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXECUTE = 1 << 2;
        const USER = 1 << 3;
        const GLOBAL = 1 << 4;
        const DEVICE = 1 << 5;
    }
}

pub(super) const VALID: u64 = 1 << 0;
pub(super) const TABLE_OR_PAGE: u64 = 1 << 1;
pub(super) const ATTR_NORMAL_WB: u64 = 0 << 2;
pub(super) const ATTR_DEVICE_NGNRNE: u64 = 1 << 2;
pub(super) const AP_USER: u64 = 1 << 6;
pub(super) const AP_READ_ONLY: u64 = 1 << 7;
pub(super) const INNER_SHAREABLE: u64 = 3 << 8;
pub(super) const ACCESS_FLAG: u64 = 1 << 10;
pub(super) const NOT_GLOBAL: u64 = 1 << 11;
pub(super) const PXN: u64 = 1 << 53;
pub(super) const UXN: u64 = 1 << 54;

/// Encode generic permissions as AArch64 stage-1 leaf attributes.
///
/// Write-only mappings are rejected because stage-1 AP cannot independently remove read access.
pub(super) fn encode(permissions: PagePermissions) -> Option<u64> {
    let device = permissions.contains(PagePermissions::DEVICE);
    if !permissions
        .intersects(PagePermissions::READ | PagePermissions::WRITE | PagePermissions::EXECUTE)
        || permissions.contains(PagePermissions::WRITE)
            && !permissions.contains(PagePermissions::READ)
        || device && permissions.intersects(PagePermissions::EXECUTE | PagePermissions::USER)
    {
        return None;
    }

    let user = permissions.contains(PagePermissions::USER);
    let execute = permissions.contains(PagePermissions::EXECUTE);
    let mut attributes = VALID
        | if device {
            ATTR_DEVICE_NGNRNE | (2 << 8)
        } else {
            ATTR_NORMAL_WB | INNER_SHAREABLE
        }
        | ACCESS_FLAG;
    if user {
        attributes |= AP_USER | PXN;
    }
    if !permissions.contains(PagePermissions::WRITE) {
        attributes |= AP_READ_ONLY;
    }
    if !execute {
        attributes |= PXN | UXN;
    } else if !user {
        attributes |= UXN;
    }
    if !permissions.contains(PagePermissions::GLOBAL) {
        attributes |= NOT_GLOBAL;
    }
    Some(attributes)
}

/// Decode a validated AArch64 leaf descriptor into generic permissions.
pub(super) fn decode(descriptor: u64) -> PagePermissions {
    let mut permissions = PagePermissions::READ;
    if descriptor & AP_READ_ONLY == 0 {
        permissions |= PagePermissions::WRITE;
    }
    let user = descriptor & AP_USER != 0;
    if user {
        permissions |= PagePermissions::USER;
        if descriptor & UXN == 0 {
            permissions |= PagePermissions::EXECUTE;
        }
    } else if descriptor & PXN == 0 {
        permissions |= PagePermissions::EXECUTE;
    }
    if descriptor & NOT_GLOBAL == 0 {
        permissions |= PagePermissions::GLOBAL;
    }
    if descriptor & (7 << 2) == ATTR_DEVICE_NGNRNE {
        permissions |= PagePermissions::DEVICE;
    }
    permissions
}

#[cfg(test)]
mod tests {
    use super::{PagePermissions, decode, encode};

    #[test]
    fn user_execute_is_never_privileged_execute() {
        let descriptor =
            encode(PagePermissions::READ | PagePermissions::EXECUTE | PagePermissions::USER)
                .unwrap();
        assert_eq!(
            decode(descriptor),
            PagePermissions::READ | PagePermissions::EXECUTE | PagePermissions::USER
        );
        assert_ne!(descriptor & super::PXN, 0);
    }

    #[test]
    fn write_without_read_is_rejected() {
        assert_eq!(encode(PagePermissions::WRITE), None);
    }

    #[test]
    fn device_mapping_uses_device_attr_and_rejects_user_or_execute() {
        let permissions = PagePermissions::READ | PagePermissions::WRITE | PagePermissions::DEVICE;
        let descriptor = encode(permissions).unwrap();
        assert_eq!(descriptor & (7 << 2), super::ATTR_DEVICE_NGNRNE);
        assert_eq!(decode(descriptor), permissions);
        assert!(encode(permissions | PagePermissions::EXECUTE).is_none());
        assert!(encode(permissions | PagePermissions::USER).is_none());
    }
}
