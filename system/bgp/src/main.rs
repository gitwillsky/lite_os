#![no_std]
#![no_main]
#![feature(allocator_ext)]
//! BGP 原生入口只装配授权工厂；每个路由域在一个线程创建并持有协议/路由 owner。
extern crate alloc;
use alloc::{sync::Arc, vec::Vec};
use native_runtime::{
    Channel, Startup,
    native_abi::{self, Status},
};
use service_directory::ServiceDirectory;
mod connection;
mod route_selection;
mod sessions;
native_runtime::entry!(main);

fn serve(channel: Channel, services: Arc<ServiceDirectory>) -> Result<(), Status> {
    service_rpc::serve(
        channel,
        1,
        b"bgp-factory",
        Arc::try_new(
            move |ordinal,
                  request: &mut service_rpc::Reader<'_>,
                  handles,
                  _: &mut service_rpc::Writer| {
                if ordinal != network_protocol::bgp::OPEN {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                let (configuration, source, minimum) =
                    network_protocol::bgp::configuration(request, handles)?;
                let mut returned = Vec::new();
                returned
                    .try_reserve_exact(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                let (client, server) = Channel::create()?;
                let services = services.clone();
                drop(native_runtime::thread::spawn(b"bgp-domain", move || {
                    if let Err(error) =
                        sessions::Sessions::new(configuration, server, &services, source, minimum)
                            .and_then(sessions::Sessions::run)
                    {
                        native_runtime::error!("BGP domain stopped: {error}");
                    }
                })?);
                returned.push(client.into_handle());
                Ok(returned)
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}
fn main(mut startup: Startup) -> i64 {
    let outgoing = Channel::from_handle(startup.take(launch_protocol::bgp::OUTGOING));
    let services = Arc::new(ServiceDirectory::new(Channel::from_handle(
        startup.take(launch_protocol::bgp::SERVICES),
    )));
    service_directory::serve_directory(
        outgoing,
        b"bgp-outgoing",
        Arc::new(move |protocol, channel| {
            if protocol != service_directory::BGP_CONFIGURATOR {
                return Err(native_abi::ERR_NOT_FOUND);
            }
            serve(channel, services.clone())
        }),
    )
    .expect("BGP outgoing service");
    native_runtime::task::exit_thread()
}
