use bitflags::bitflags;

bitflags! {
    /// @description Generic memory domain 可表达的 architecture-neutral mapping 权限。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct PagePermissions: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXECUTE = 1 << 2;
        const USER = 1 << 3;
        const GLOBAL = 1 << 4;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct RiscvPteFlags: u8 {
        const V = 1 << 0;
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
        const G = 1 << 5;
        const A = 1 << 6;
        const D = 1 << 7;
    }
}

/// @description 将语义 mapping permissions 编码为合法 RISC-V leaf PTE flags。
/// @param permissions generic memory domain 请求的权限。
/// @return 合法编码；write-without-read 在 RISC-V 上不可表达时返回 `None`。
pub(super) fn encode(permissions: PagePermissions) -> Option<RiscvPteFlags> {
    if permissions.contains(PagePermissions::WRITE) && !permissions.contains(PagePermissions::READ)
    {
        return None;
    }
    let mut flags = RiscvPteFlags::V | RiscvPteFlags::A;
    if permissions.contains(PagePermissions::READ) {
        flags |= RiscvPteFlags::R;
    }
    if permissions.contains(PagePermissions::WRITE) {
        flags |= RiscvPteFlags::W | RiscvPteFlags::D;
    }
    if permissions.contains(PagePermissions::EXECUTE) {
        flags |= RiscvPteFlags::X;
    }
    if permissions.contains(PagePermissions::USER) {
        flags |= RiscvPteFlags::U;
    }
    if permissions.contains(PagePermissions::GLOBAL) {
        flags |= RiscvPteFlags::G;
    }
    Some(flags)
}

/// @description 从已验证 RISC-V leaf PTE 投影语义 mapping permissions。
/// @param flags backend 私有 raw PTE flags。
/// @return 不包含 valid/accessed/dirty encoding bit 的语义权限。
pub(super) fn decode(flags: RiscvPteFlags) -> PagePermissions {
    let mut permissions = PagePermissions::empty();
    if flags.contains(RiscvPteFlags::R) {
        permissions |= PagePermissions::READ;
    }
    if flags.contains(RiscvPteFlags::W) {
        permissions |= PagePermissions::WRITE;
    }
    if flags.contains(RiscvPteFlags::X) {
        permissions |= PagePermissions::EXECUTE;
    }
    if flags.contains(RiscvPteFlags::U) {
        permissions |= PagePermissions::USER;
    }
    if flags.contains(RiscvPteFlags::G) {
        permissions |= PagePermissions::GLOBAL;
    }
    permissions
}

#[cfg(test)]
mod tests {
    use super::{PagePermissions, RiscvPteFlags, decode, encode};

    #[test]
    fn semantic_permissions_round_trip_through_riscv_encoding() {
        let permissions = PagePermissions::READ
            | PagePermissions::WRITE
            | PagePermissions::USER
            | PagePermissions::GLOBAL;
        let encoded = encode(permissions).unwrap();
        assert!(encoded.contains(RiscvPteFlags::V | RiscvPteFlags::A | RiscvPteFlags::D));
        assert_eq!(decode(encoded), permissions);
    }

    #[test]
    fn riscv_backend_rejects_write_without_read() {
        assert_eq!(encode(PagePermissions::WRITE), None);
    }
}
