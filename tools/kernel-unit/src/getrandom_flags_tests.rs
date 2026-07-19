use crate::getrandom_flags::{
    GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, getrandom_flags_supported,
};

#[test]
fn rust_std_insecure_entropy_request_is_accepted() {
    assert!(getrandom_flags_supported(GRND_INSECURE));
    assert!(getrandom_flags_supported(GRND_INSECURE | GRND_NONBLOCK));
}

#[test]
fn insecure_and_random_sources_are_mutually_exclusive() {
    assert!(!getrandom_flags_supported(GRND_INSECURE | GRND_RANDOM));
}

#[test]
fn unknown_getrandom_flags_remain_invalid() {
    assert!(!getrandom_flags_supported(0x8));
}
