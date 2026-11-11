#![no_std]
#![no_main]

//! Authorized per-interface DHCPv4 service, driven by the existing packet socket and port timers.

extern crate alloc;
#[macro_use]
extern crate native_runtime;

use alloc::{sync::Arc, vec::Vec};
use dhcp_server::{
    ARP_BYTES, Action, DATAGRAM_BYTES, Delivery, Error, Pool, Reply, Server, arp_probe,
};
use fallible_tree::FallibleMap;
use native_runtime::{
    Channel, Event, Handle, Port, Startup,
    native_abi::{self, Status, signals},
    sync::Mutex,
    system::monotonic,
};
use network_protocol::{
    NetworkClient, PacketAddress, PacketKind, RemoteNamespace, RemotePacketSocket, SocketError,
};
use router_protocol::dhcp_server as protocol;
use service_directory::ServiceDirectory;

native_runtime::entry!(main);

const ETH_IPV4: u16 = 0x0800;
const ETH_ARP: u16 = 0x0806;
const FIRST_RX: u64 = 4;
const CONTROL: u64 = 1;
const NETWORK_CLOSED: u64 = 2;
const RESUME: u64 = 3;
const PACKET_BATCH: usize = 16;

struct Directory {
    services: ServiceDirectory,
    // Owns interface claims across factory connections. Without this, two packet sockets could
    // serve independent pools on the same interface during replacement or a concurrent request.
    occupied: Mutex<FallibleMap<u32, ()>>,
}

struct Claim {
    directory: Arc<Directory>,
    index: u32,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.directory.occupied.lock().remove(&self.index);
    }
}

fn main(mut startup: Startup) -> i64 {
    use launch_protocol::dhcp_server;
    let directory = Arc::new(Directory {
        services: ServiceDirectory::new(Channel::from_handle(startup.take(dhcp_server::SERVICES))),
        occupied: Mutex::new(FallibleMap::new()),
    });
    let outgoing = Channel::from_handle(startup.take(dhcp_server::OUTGOING));
    service_directory::serve_directory(
        outgoing,
        b"dhcp-server-outgoing",
        Arc::new(move |name: &[u8], channel: Channel| {
            if name != service_directory::DHCP_SERVER {
                return Err(native_abi::ERR_NOT_FOUND);
            }
            let directory = directory.clone();
            service_rpc::serve(
                channel,
                1,
                b"dhcp-server",
                Arc::new(
                    move |ordinal: u32,
                          request: &mut service_rpc::Reader<'_>,
                          handles: Vec<Handle>,
                          _: &mut service_rpc::Writer| {
                        if ordinal != protocol::START {
                            return Err(native_abi::ERR_NOT_SUPPORTED);
                        }
                        start(&directory, request, handles)
                    },
                ),
            )
        }),
    )
    .expect("dhcp-server outgoing service");
    native_runtime::task::exit_thread()
}

fn start(
    directory: &Arc<Directory>,
    request: &mut service_rpc::Reader<'_>,
    handles: Vec<Handle>,
) -> Result<Vec<Handle>, Status> {
    let index = request.u32()?;
    let pool = Pool::decode(request.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    request.finish()?;
    if !handles.is_empty() || index <= 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    {
        let mut occupied = directory.occupied.lock();
        if occupied.get(&index).is_some() {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        occupied
            .try_insert(index, ())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    }
    let claim = Claim {
        directory: directory.clone(),
        index,
    };
    let (control, peer) = Channel::create()?;
    let session = Session::open(claim, pool, control)?;
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    drop(native_runtime::thread::spawn(
        b"dhcp-server-interface",
        move || {
            if let Err(status) = session.run()
                && status != native_abi::ERR_PEER_CLOSED
            {
                warn!("dhcp-server: interface {index} stopped: {status}");
            }
        },
    )?);
    returned.push(peer.into_handle());
    Ok(returned)
}

struct Input {
    socket: RemotePacketSocket,
    readiness: Event,
    key: u64,
    // Each receive observer is one-shot. Rearming it early would queue duplicate wakeups.
    armed: bool,
}

impl Input {
    fn open(
        network: &RemoteNamespace,
        index: u32,
        key: u64,
        protocol: u16,
    ) -> Result<Self, Status> {
        let endpoint = network
            .packet_socket(PacketKind::Datagram, protocol.to_be())
            .map_err(socket_status)?;
        let socket = RemotePacketSocket::new(endpoint.channel).map_err(socket_status)?;
        socket
            .bind(link(index, [0; 6], protocol))
            .map_err(socket_status)?;
        Ok(Self {
            socket,
            readiness: endpoint.readiness,
            key,
            armed: false,
        })
    }
}

struct Session {
    // Disabled pools retain occupancy and deadlines without subscribing to link traffic.
    input: Option<Input>,
    arp: Option<Input>,
    mac: [u8; 6],
    next_rx_key: u64,
    control: Channel,
    port: Port,
    server: Server,
    // The staging owner holds the opposite end until commit or rollback. Acknowledging a
    // pause on this actor means all earlier datagrams completed before interface mutation.
    pause: Option<Channel>,
    // Retains the namespace and the single interface claim until the socket is closed.
    network: RemoteNamespace,
    claim: Claim,
}

impl Session {
    fn open(claim: Claim, pool: Pool, control: Channel) -> Result<Self, Status> {
        let client = NetworkClient::new(
            claim
                .directory
                .services
                .connect(service_directory::NETWORK)?,
        );
        let network = client.create_namespace(true)?;
        let mac = validate_link(&network, claim.index, &pool)?;
        let input = Input::open(&network, claim.index, FIRST_RX, ETH_IPV4)?;
        let arp = if pool.options.probe_addresses {
            Some(Input::open(&network, claim.index, FIRST_RX + 1, ETH_ARP)?)
        } else {
            None
        };
        let server = Server::new(pool).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let port = Port::create()?;
        control.wait_async(&port, CONTROL, signals::READABLE | signals::PEER_CLOSED)?;
        network
            .channel()
            .wait_async(&port, NETWORK_CLOSED, signals::PEER_CLOSED)?;
        Ok(Self {
            input: Some(input),
            arp,
            mac,
            next_rx_key: FIRST_RX + 2,
            control,
            port,
            server,
            pause: None,
            network,
            claim,
        })
    }

    fn run(mut self) -> Result<(), Status> {
        loop {
            self.server.expire(now_ms());
            if self.input.is_none() && self.pause.is_none() && self.server.next_expiry().is_none() {
                return Ok(());
            }
            let mut pending = false;
            if self.input.is_some() && self.pause.is_none() {
                // Process queued conflicts before considering any probe successful. Both
                // inputs yield to the control port after a bounded batch.
                let arp_pending = self.arp.is_some() && self.drain(ETH_ARP)?;
                if !arp_pending {
                    let mut output = [0; DATAGRAM_BYTES];
                    let now = now_ms();
                    for _ in 0..PACKET_BATCH {
                        let Some(reply) = self
                            .server
                            .finish_check(now, &mut output)
                            .map_err(|_| native_abi::ERR_IO)?
                        else {
                            break;
                        };
                        self.reply(reply)?;
                    }
                }
                pending = self.drain(ETH_IPV4)? || arp_pending;
            }
            let deadline = if pending {
                monotonic()
            } else {
                self.server
                    .next_expiry()
                    .into_iter()
                    .chain(self.server.next_check())
                    .min()
                    .map_or(native_abi::DEADLINE_INFINITE, |ms| {
                        ms.saturating_mul(1_000_000).min(i64::MAX as u64) as i64
                    })
            };
            match self.port.wait(deadline) {
                Ok(packet) if packet.key >= FIRST_RX => {
                    if let Some(input) = self
                        .input
                        .iter_mut()
                        .chain(self.arp.iter_mut())
                        .find(|input| input.key == packet.key)
                    {
                        input.armed = false;
                        input.readiness.signal(signals::USER_0, 0)?;
                    }
                }
                Ok(packet) if packet.key == CONTROL => {
                    if packet.observed & signals::PEER_CLOSED != 0 {
                        return Ok(());
                    }
                    self.dispatch()?;
                    self.control.wait_async(
                        &self.port,
                        CONTROL,
                        signals::READABLE | signals::PEER_CLOSED,
                    )?;
                }
                Ok(packet) if packet.key == RESUME => {
                    if self.pause.as_ref().is_some_and(|pause| {
                        pause.wait_one(signals::PEER_CLOSED, 0) != Err(native_abi::ERR_TIMED_OUT)
                    }) {
                        self.pause = None;
                    }
                }
                Ok(_) => return Err(native_abi::ERR_PEER_CLOSED),
                Err(native_abi::ERR_TIMED_OUT) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn dispatch(&mut self) -> Result<(), Status> {
        let message = self.control.read()?;
        let mut handles = message.handles.into_iter();
        let Some(reply) = handles.next().map(Channel::from_handle) else {
            return Ok(());
        };
        let bytes = message.bytes.as_slice();
        let Some((command, body)) = bytes.split_first_chunk::<4>() else {
            return Ok(());
        };
        let command = u32::from_le_bytes(*command);
        let result = match command {
            protocol::PAUSE if body.is_empty() => match (handles.next(), handles.next()) {
                (Some(token), None) => self.pause(Channel::from_handle(token)),
                _ => Err(native_abi::ERR_INVALID_ARGS),
            },
            protocol::APPLY if handles.next().is_none() => self.apply(body),
            _ => {
                let now = now_ms();
                if handles.next().is_none()
                    && let Ok(after) = protocol::decode_query(bytes)
                    && let Ok(bytes) = protocol::encode_page(now, self.server.bindings(after, now))
                {
                    let _ = reply.write(&bytes, Vec::new());
                }
                return Ok(());
            }
        };
        let status = result.err().unwrap_or(native_abi::OK);
        let _ = reply.write(&status.to_le_bytes(), Vec::new());
        Ok(())
    }

    fn pause(&mut self, token: Channel) -> Result<(), Status> {
        if self.pause.as_ref().is_some_and(|pause| {
            pause.wait_one(signals::PEER_CLOSED, 0) == Err(native_abi::ERR_TIMED_OUT)
        }) {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        token.wait_async(&self.port, RESUME, signals::PEER_CLOSED)?;
        self.server.cancel_checks();
        self.pause = Some(token);
        Ok(())
    }

    fn apply(&mut self, body: &[u8]) -> Result<(), Status> {
        if self.pause.is_none() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        if body.is_empty() {
            self.input = None;
            self.arp = None;
            return Ok(());
        }
        let pool = Pool::decode(body).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let mac = validate_link(&self.network, self.claim.index, &pool)?;
        let input = if self.input.is_none() {
            Some(self.open_input(ETH_IPV4)?)
        } else {
            None
        };
        let arp = match (pool.options.probe_addresses, self.arp.is_some()) {
            (true, false) => Some(self.open_input(ETH_ARP)?),
            _ => None,
        };
        let check = pool.options.probe_addresses;
        self.server
            .replace(pool)
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        if let Some(input) = input {
            self.input = Some(input);
        }
        if let Some(arp) = arp {
            self.arp = Some(arp);
        } else if !check {
            self.arp = None;
        }
        self.mac = mac;
        Ok(())
    }

    fn open_input(&mut self, protocol: u16) -> Result<Input, Status> {
        let key = self.next_rx_key;
        self.next_rx_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        Input::open(&self.network, self.claim.index, key, protocol)
    }

    fn drain(&mut self, protocol: u16) -> Result<bool, Status> {
        let mut pending = true;
        for _ in 0..PACKET_BATCH {
            if !self.receive(protocol)? {
                pending = false;
                break;
            }
        }
        let input = match protocol {
            ETH_ARP => &mut self.arp,
            _ => &mut self.input,
        }
        .as_mut()
        .expect("active packet input");
        if !input.armed {
            input
                .readiness
                .wait_async(&self.port, input.key, signals::USER_0)?;
            input.armed = true;
        }
        Ok(pending)
    }

    fn receive(&mut self, protocol: u16) -> Result<bool, Status> {
        let input = match protocol {
            ETH_ARP => &self.arp,
            _ => &self.input,
        }
        .as_ref()
        .expect("active packet input");
        let received = match input.socket.receive(
            if protocol == ETH_ARP {
                ARP_BYTES
            } else {
                65_535
            },
            false,
        ) {
            Ok(received) => received,
            Err(SocketError::Again) => return Ok(false),
            Err(error) => return Err(socket_status(error)),
        };
        // ARP is fixed-size and permits Ethernet padding beyond its payload.
        if (protocol == ETH_IPV4 && received.full_length != received.data.len())
            || received.source.interface_index as u32 != self.claim.index
            || received.source.address_length != 6
        {
            return Ok(true);
        }
        let source = <[u8; 6]>::try_from(&received.source.address[..6]).expect("fixed MAC");
        let mut output = [0; DATAGRAM_BYTES];
        let now = now_ms();
        let action = if protocol == ETH_ARP {
            self.server
                .observe_arp(&received.data, source, self.mac, now, &mut output)
        } else {
            self.server.handle(&received.data, source, now, &mut output)
        };
        match action {
            Ok(Some(Action::Reply(reply))) => self.reply(reply)?,
            Ok(Some(Action::Probe(address))) => {
                let mut probe = [0; ARP_BYTES];
                arp_probe(address, self.mac, &mut probe).map_err(|_| native_abi::ERR_IO)?;
                let socket = &self.arp.as_ref().expect("probing enabled").socket;
                let result =
                    socket.send_to(&probe, Some(link(self.claim.index, [0xff; 6], ETH_ARP)));
                match result {
                    Ok(ARP_BYTES) => self.server.probe_transmitted(address, Some(now_ms())),
                    Ok(_) | Err(SocketError::Again | SocketError::NoBuffer) => {
                        self.server.probe_transmitted(address, None);
                    }
                    Err(error) => return Err(socket_status(error)),
                }
            }
            Err(Error::NoMemory) => warn!("dhcp-server: allocation failed on {}", self.claim.index),
            _ => {}
        }
        Ok(true)
    }

    fn reply(&self, reply: Reply<'_>) -> Result<(), Status> {
        let socket = &self
            .input
            .as_ref()
            .expect("active DHCP packet input")
            .socket;
        let mac = match reply.delivery {
            Delivery::Broadcast => [0xff; 6],
            Delivery::Unicast { mac, .. } => mac,
        };
        match socket.send_to(reply.bytes, Some(link(self.claim.index, mac, ETH_IPV4))) {
            Ok(_) | Err(SocketError::Again | SocketError::NoBuffer) => Ok(()),
            Err(error) => Err(socket_status(error)),
        }
    }
}

fn validate_link(network: &RemoteNamespace, index: u32, pool: &Pool) -> Result<[u8; 6], Status> {
    let snapshot = network.interface_snapshot(index).map_err(socket_status)?;
    if snapshot.loopback
        || !snapshot.up
        || snapshot.mac == [0; 6]
        || snapshot.mac[0] & 1 != 0
        || snapshot.mtu < DATAGRAM_BYTES as i32
        || snapshot.address != Some(pool.server)
        || snapshot.prefix_length != pool.prefix_length
    {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(snapshot.mac)
}

fn link(index: u32, mac: [u8; 6], protocol: u16) -> PacketAddress {
    let mut address = [0; 8];
    address[..6].copy_from_slice(&mac);
    PacketAddress {
        protocol: protocol.to_be(),
        interface_index: index as i32,
        hardware_type: 1,
        packet_type: 0,
        address_length: 6,
        address,
    }
}

fn now_ms() -> u64 {
    (monotonic().max(0) as u64) / 1_000_000
}

fn socket_status(error: SocketError) -> Status {
    match error {
        SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        SocketError::NetworkDown | SocketError::BrokenPipe => native_abi::ERR_PEER_CLOSED,
        SocketError::NoDevice | SocketError::DeviceNotAvailable => native_abi::ERR_NOT_FOUND,
        _ => native_abi::ERR_IO,
    }
}
