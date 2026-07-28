use quickjs_runtime::EngineError;

pub(super) fn parse_u32(value: Option<&str>, name: &str) -> Result<u32, EngineError> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| EngineError::from_host(format!("invalid {name}")))
}

pub(super) fn parse_u64(value: Option<&str>, name: &str) -> Result<u64, EngineError> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| EngineError::from_host(format!("invalid {name}")))
}

pub(super) fn parse_i32(value: Option<&str>, name: &str) -> Result<i32, EngineError> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| EngineError::from_host(format!("invalid {name}")))
}
