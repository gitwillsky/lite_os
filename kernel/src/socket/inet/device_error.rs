/// 把 adapter result 分类为 smoltcp 的 available/would-block/error 三态。
pub(super) fn classify_optional<T, E>(
    result: Result<T, E>,
    is_would_block: impl FnOnce(&E) -> bool,
) -> Result<Option<T>, E> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_would_block(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::classify_optional;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum InjectedError {
        WouldBlock,
        Device,
    }

    #[test]
    fn device_failure_receive_is_returned_instead_of_panicking() {
        let outcome = std::panic::catch_unwind(|| {
            classify_optional(Err::<(), _>(InjectedError::Device), |error| {
                *error == InjectedError::WouldBlock
            })
        });
        assert!(matches!(outcome, Ok(Err(InjectedError::Device))));
    }

    #[test]
    fn device_failure_transmit_is_returned_instead_of_panicking() {
        let outcome = std::panic::catch_unwind(|| Err::<(), _>(InjectedError::Device));
        assert!(matches!(outcome, Ok(Err(InjectedError::Device))));
    }

    #[test]
    fn would_block_remains_an_empty_optional_result() {
        assert_eq!(
            classify_optional(Err::<(), _>(InjectedError::WouldBlock), |error| {
                *error == InjectedError::WouldBlock
            }),
            Ok(None)
        );
    }

    #[test]
    fn successful_adapter_value_is_preserved() {
        assert_eq!(
            classify_optional(Ok::<_, InjectedError>(13), |_| false),
            Ok(Some(13))
        );
    }
}
