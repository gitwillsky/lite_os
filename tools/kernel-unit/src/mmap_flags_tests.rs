use crate::mmap_flags::{
    MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE, MAP_SHARED, MAP_STACK,
    mmap_flags_supported,
};

#[test]
fn rust_std_stack_mapping_is_an_accepted_advisory_variant() {
    assert!(mmap_flags_supported(
        MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK
    ));
}

#[test]
fn mmap_requires_one_sharing_mode_and_rejects_unknown_bits() {
    assert!(!mmap_flags_supported(MAP_ANONYMOUS | MAP_STACK));
    assert!(!mmap_flags_supported(
        MAP_PRIVATE | MAP_SHARED | MAP_ANONYMOUS
    ));
    assert!(!mmap_flags_supported(MAP_PRIVATE | MAP_ANONYMOUS | 0x400));
}

#[test]
fn fixed_replacement_modes_remain_mutually_exclusive() {
    assert!(!mmap_flags_supported(
        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | MAP_FIXED_NOREPLACE
    ));
}
