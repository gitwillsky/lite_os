#![no_std]
#![no_main]

//! Native forwarding executor, activated only through the router configuration capability.

extern crate alloc;

mod session;
mod socket;
mod upstream;

use alloc::{sync::Arc, vec::Vec};
use fallible_tree::FallibleMap;
use native_runtime::{
    Channel, Handle, Startup,
    native_abi::{self, Status},
    sync::Mutex,
    warn,
};
use network_protocol::RemoteNamespace;
use router_protocol::dns_cache::{START, Settings};

use session::Session;

native_runtime::entry!(main);

struct Factory {
    // The configuration owner delegates duplicates of its namespace channel. An exclusive
    // per-channel claim prevents concurrent policies from binding listeners in the same domain.
    active: Mutex<FallibleMap<u64, ()>>,
}

struct Claim(Arc<Factory>, u64);

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.active.lock().remove(&self.1);
    }
}

fn main(mut startup: Startup) -> i64 {
    use launch_protocol::dns_cache;
    let factory = Arc::new(Factory {
        active: Mutex::new(FallibleMap::new()),
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
                        let [network]: [Handle; 1] = handles
                            .try_into()
                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        let settings = Settings::decode(request.bytes()?)
                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        request.finish()?;
                        start(&factory, Channel::from_handle(network), settings)
                    },
                ),
            )
        }),
    )
    .expect("DNS forwarding outgoing directory");
    native_runtime::task::exit_thread()
}

fn start(
    factory: &Arc<Factory>,
    network: Channel,
    settings: Settings,
) -> Result<Vec<Handle>, Status> {
    let key = network.basic()?.koid;
    {
        let mut active = factory.active.lock();
        if active.get(&key).is_some() {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        active
            .try_insert(key, ())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    }
    let claim = Claim(factory.clone(), key);
    let network = RemoteNamespace::from_channel(network);
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
