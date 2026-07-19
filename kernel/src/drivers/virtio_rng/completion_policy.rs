//! @description VirtIO entropy used-ring length/generation validation policy。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletionValidity {
    Initialized(usize),
    Corrupt,
}

pub(super) fn validate_completion(requested: usize, returned: usize) -> CompletionValidity {
    if returned == 0 || returned > requested {
        CompletionValidity::Corrupt
    } else {
        CompletionValidity::Initialized(returned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_current_nonempty_bounded_prefix_becomes_initialized() {
        assert_eq!(
            validate_completion(4096, 1024),
            CompletionValidity::Initialized(1024)
        );
        for corrupt in [
            validate_completion(4096, 0),
            validate_completion(4096, 4097),
        ] {
            assert_eq!(corrupt, CompletionValidity::Corrupt);
        }
    }
}
