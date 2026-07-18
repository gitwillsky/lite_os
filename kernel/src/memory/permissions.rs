use bitflags::bitflags;

use crate::arch::mmu::PagePermissions;

bitflags! {
    /// @description VMA 的 architecture-neutral access policy。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct MapPermission: u8 {
        const R = 1 << 0;
        const W = 1 << 1;
        const X = 1 << 2;
        const U = 1 << 3;
    }
}

impl From<MapPermission> for PagePermissions {
    fn from(permission: MapPermission) -> Self {
        let mut result = Self::empty();
        if permission.contains(MapPermission::R) {
            result |= Self::READ;
        }
        if permission.contains(MapPermission::W) {
            result |= Self::WRITE;
        }
        if permission.contains(MapPermission::X) {
            result |= Self::EXECUTE;
        }
        if permission.contains(MapPermission::U) {
            result |= Self::USER;
        }
        result
    }
}
