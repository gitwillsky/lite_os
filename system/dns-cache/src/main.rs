#![no_std]
#![no_main]

//! Native forwarding executor, activated only through the router configuration capability.

extern crate alloc;

mod session;
mod socket;
mod upstream;

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use native_runtime::{
    Channel, Handle, Startup,
    native_abi::{self, Status},
    warn,
};
use network_protocol::NetworkClient;
use router_protocol::dns_cache::{START, Settings};
use service_directory::ServiceDirectory;

use session::Session;

native_runtime::entry!(main);

struct Factory {
    services: ServiceDirectory,
    // Only one active policy may bind listeners in this network domain. The admitted session
    // releases the claim after closing its sockets. Concurrent START must not create a rival.
    active: AtomicBool,
}

struct Claim(Arc<Factory>);

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Release);
    }
}

fn main(mut startup: Startup) -> i64 {
    use launch_protocol::dns_cache;
    let factory = Arc::new(Factory {
        services: ServiceDirectory::new(Channel::from_handle(startup.take(dns_cache::SERVICES))),
        active: AtomicBool::new(false),
    });
    let outgoing = Channel::from_handle(startup.take(dns_cache::OUTGOING));
    service_directory::serve_directory(
        outgoing,
        b"dns-cache-outgoing",
        Arc::new(move |name: &[u8], channel: Channel| {
            if name != service_directory::DNS_CACHE {
                return Err(native_abi::ERR_NOT_FOUND);
            }
            let factory = factory.clone();
            service_rpc::serve(
                channel,
                1,
                b"dns-cache",
                Arc::new(
                    move |ordinal: u32,
                          request: &mut service_rpc::Reader<'_>,
                          handles: Vec<Handle>,
                          _: &mut service_rpc::Writer| {
                        if ordinal != START {
                            return Err(native_abi::ERR_NOT_SUPPORTED);
                        }
                        if !handles.is_empty() {
                            return Err(native_abi::ERR_INVALID_ARGS);
                        }
                        let settings = Settings::decode(request.bytes()?)
                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        request.finish()?;
                        start(&factory, settings)
                    },
                ),
            )
        }),
    )
    .expect("DNS forwarding outgoing directory");
    native_runtime::task::exit_thread()
}

fn start(factory: &Arc<Factory>, settings: Settings) -> Result<Vec<Handle>, Status> {
    factory
        .active
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .map_err(|_| native_abi::ERR_ALREADY_BOUND)?;
    let claim = Claim(factory.clone());
    let network = NetworkClient::new(factory.services.connect(service_directory::NETWORK)?)
        .create_namespace(true)?;
    let (control, peer) = Channel::create()?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let session = Session::open(network, settings, control)?;
    drop(native_runtime::thread::spawn(
        b"dns-cache-session",
        move || {
            let _claim = claim;
            if let Err(error) = session.run()
                && error != native_abi::ERR_PEER_CLOSED
            {
                warn!("dns-cache: forwarding session exited: {error}");
            }
        },
    )?);
    result.push(peer.into_handle());
    Ok(result)
}
