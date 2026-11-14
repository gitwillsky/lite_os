//! 只读配置投影，与正式准备使用相同桥正文。通过安装者权限传递，不开放给应用。
use super::*;
use router_protocol::{
    interface::{InterfaceConfiguration, InterfaceConfigurationBatch},
    switching::Ports,
};

pub(crate) const PROJECT_BRIDGES: u32 = 13;
const VERSION: u32 = 1;

pub(crate) fn dispatch(
    request: &mut Reader<'_>,
    mut handles: Vec<Handle>,
    reply: &mut Writer,
    project: impl FnOnce(Ports<'_>, InterfaceConfigurationBatch<'_>) -> Result<Vec<u8>, Status>,
) -> Result<Vec<Handle>, Status> {
    if request.u32()? != VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let length = request.u64()?;
    let interface_length = request.u64()?;
    request.finish()?;
    if length == 0 || handles.len() != 1 + usize::from(interface_length != 0) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let bytes = Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?).read_bytes(length)?;
    let ports = Ports::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let interfaces = if interface_length == 0 {
        None
    } else {
        Some(
            Vmo::from_handle(service_rpc::take_handle(&mut handles, 1)?)
                .read_bytes(interface_length)?,
        )
    };
    let plans = interfaces
        .as_deref()
        .map(InterfaceConfigurationBatch::parse)
        .transpose()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        .unwrap_or_default();
    let projected = project(ports, plans)?;
    reply.try_reserve(8)?;
    reply.u64(projected.len() as u64);
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    returned.push(Vmo::from_bytes(&projected)?.into_handle());
    Ok(returned)
}

pub(super) fn request(
    channel: &Channel,
    ports: Ports<'_>,
    plans: InterfaceConfigurationBatch<'_>,
) -> Result<Vec<u8>, Status> {
    let mut writer = Writer::request(PROJECT_BRIDGES);
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(2)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    handles.push(Vmo::from_bytes(ports.bytes())?.into_handle());
    let interface_bytes = plans.bytes();
    if !interface_bytes.is_empty() {
        handles.push(Vmo::from_bytes(interface_bytes)?.into_handle());
    }
    writer.try_reserve(20)?;
    writer
        .u32(VERSION)
        .u64(ports.bytes().len() as u64)
        .u64(interface_bytes.len() as u64);
    let mut reply = call(channel, &writer, handles)?;
    let mut reader = reply.reader();
    let length = reader.u64()?;
    reader.finish()?;
    if length != ports.bytes().len() as u64 || reply.handles.len() != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let bytes =
        Vmo::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?).read_bytes(length)?;
    let resolved = Ports::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    if !resolved.projected() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(bytes)
}

pub(crate) fn encode_plans(plans: &[InterfaceConfiguration<'_>]) -> Result<Vec<u8>, Status> {
    let mut bytes = Vec::new();
    if !plans.is_empty() {
        let length = InterfaceConfigurationBatch::encoded_size(plans)
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        bytes
            .try_reserve_exact(length)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.resize(length, 0);
        InterfaceConfigurationBatch::encode(plans, &mut bytes)
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    }
    Ok(bytes)
}
