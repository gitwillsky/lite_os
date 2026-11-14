#![no_std]
#![no_main]
#![feature(allocator_ext)]
//! 原生 OSPF 服务入口。每个实例的网络端点和协议状态由一个线程推进。

extern crate alloc;

use alloc::{sync::Arc, vec};
use native_runtime::{
    Channel, Startup,
    native_abi::{self, Status},
};

mod adjacency;
mod authentication;
mod database;
mod discovery;
mod origination;
mod socket;
mod spf;

native_runtime::entry!(main);

fn serve(channel: Channel) -> Result<(), Status> {
    service_rpc::serve(
        channel,
        1,
        b"ospf-factory",
        Arc::try_new(
            move |ordinal,
                  reader: &mut service_rpc::Reader<'_>,
                  handles,
                  _: &mut service_rpc::Writer| {
                if ordinal != network_protocol::ospf::OPEN {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                let (configuration, source, network) =
                    network_protocol::ospf::configuration(reader, handles)?;
                let (client, server) = Channel::create()?;
                drop(native_runtime::thread::spawn(b"ospf-domain", move || {
                    if let Err(error) = discovery::run(configuration, source, server, network) {
                        native_runtime::warn!("OSPF instance stopped: {error}");
                    }
                })?);
                Ok(vec![client.into_handle()])
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}

fn main(mut startup: Startup) -> i64 {
    let outgoing = Channel::from_handle(startup.take(launch_protocol::ospf::OUTGOING));
    service_directory::serve_directory(
        outgoing,
        b"ospf-outgoing",
        Arc::new(move |protocol, channel| {
            if protocol != service_directory::OSPF_CONFIGURATOR {
                return Err(native_abi::ERR_NOT_FOUND);
            }
            serve(channel)
        }),
    )
    .expect("OSPF outgoing service");
    native_runtime::task::exit_thread()
}
