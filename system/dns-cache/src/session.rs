//! One DNS policy owns its listeners, cache, transactions, and all endpoint cleanup.

use alloc::vec::Vec;
use core::{net::SocketAddrV6, ops::Range};

use dns_cache::{Cache, Query};
use native_runtime::{
    Channel, Port,
    native_abi::{self, Status, signals},
    system::monotonic,
};
use network_protocol::{InetAddress, InetOptionChange, InetProtocol, RemoteNamespace, SocketError};
use router_protocol::dns_cache::{self as protocol, Binding, DNS_PORT, Settings};

use crate::{
    socket::{DNS_MAX, Socket, StreamInput, StreamOutput},
    upstream::{Attempt, Progress, UDP_LIMIT},
};

const CONTROL: u64 = 1;
const NETWORK: u64 = 2;
const RESUME: u64 = 3;
const FIRST_SOCKET: u64 = 4;
const BATCH: usize = 16;
const MAX_CLIENTS: usize = 64;
const MAX_TRANSACTIONS: usize = 128;
const CLIENT_IDLE: i64 = 10_000_000_000;
const QUERY_BUDGET: i64 = 6_000_000_000;

struct Listener {
    key: u64,
    binding: Binding,
    udp: Socket,
    tcp: Socket,
}

impl Listener {
    fn open(network: &RemoteNamespace, binding: Binding, key: u64) -> Result<Self, SocketError> {
        let interface = network.interface_snapshot(binding.interface)?;
        let addresses = network.address_snapshot(binding.interface)?;
        if !interface.up
            || !(0..addresses.len())
                .filter_map(|index| addresses.get(index))
                .any(|row| row.address == binding.address && row.usable)
        {
            return Err(SocketError::AddressNotAvailable);
        }
        let name = interface
            .name
            .split(|&byte| byte == 0)
            .next()
            .unwrap_or_default();
        let address = scoped(
            InetAddress::new(binding.address, DNS_PORT),
            binding.interface,
        );
        let open = |protocol| {
            let socket = Socket::open(network, address, protocol)?;
            socket.endpoint.bind_to_device(name, true)?;
            if address.is_ipv6() {
                socket
                    .endpoint
                    .set_option(InetOptionChange::Ipv6Only(true))?;
            }
            if protocol == InetProtocol::Tcp {
                socket
                    .endpoint
                    .set_option(InetOptionChange::ReuseAddress(true))?;
            }
            socket.endpoint.bind(address)?;
            Ok(socket)
        };
        let udp = open(InetProtocol::Udp)?;
        let tcp = open(InetProtocol::Tcp)?;
        tcp.endpoint.listen(BATCH)?;
        Ok(Self {
            key,
            binding,
            udp,
            tcp,
        })
    }
}

#[derive(Clone, Copy)]
enum Target {
    Udp { listener: usize, peer: InetAddress },
    Tcp(u64),
}
impl Target {
    fn udp_limit(self) -> Option<usize> {
        matches!(self, Self::Udp { .. }).then_some(UDP_LIMIT)
    }

    fn use_tcp(self, length: usize) -> bool {
        self.udp_limit().is_none_or(|limit| length > limit)
    }
}

enum ClientState {
    Reading,
    Waiting,
    Writing(StreamOutput),
}

struct Client {
    key: u64,
    socket: Socket,
    state: ClientState,
    input: StreamInput,
    expires: i64,
}

struct Transaction {
    key: u64,
    target: Target,
    request: Vec<u8>,
    upstream: usize,
    attempt: Attempt,
    expires: i64,
}

pub(super) struct Session {
    listeners: Vec<Listener>,
    clients: Vec<Client>,
    transactions: Vec<Transaction>,
    network: RemoteNamespace,
    control: Channel,
    pause: Option<Channel>,
    port: Port,
    upstream: Vec<InetAddress>,
    cache: Cache,
    output: Vec<u8>,
    next_key: u64,
}

impl Session {
    pub(super) fn open(
        network: RemoteNamespace,
        settings: Settings,
        control: Channel,
    ) -> Result<Self, Status> {
        let port = Port::create()?;
        network
            .channel()
            .wait_async(&port, NETWORK, signals::PEER_CLOSED)?;
        let mut session = Self {
            listeners: Vec::new(),
            clients: Vec::new(),
            transactions: Vec::new(),
            network,
            control,
            pause: None,
            port,
            upstream: Vec::new(),
            cache: Cache::new(2048, 2 * 1024 * 1024),
            output: Vec::new(),
            next_key: FIRST_SOCKET,
        };
        session.apply(settings)?;
        Ok(session)
    }

    fn apply(&mut self, settings: Settings) -> Result<(), Status> {
        // Reserve and bind the full set of additions before retiring a live listener.
        let count = settings
            .listen
            .iter()
            .filter(|binding| {
                !self
                    .listeners
                    .iter()
                    .any(|listener| &listener.binding == *binding)
            })
            .count();
        self.listeners
            .try_reserve(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut additions = Vec::new();
        additions
            .try_reserve_exact(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for &binding in &settings.listen {
            if self
                .listeners
                .iter()
                .any(|listener| listener.binding == binding)
            {
                continue;
            }
            let key = self.key()?;
            let tcp_key = self.key()?;
            let mut listener = Listener::open(&self.network, binding, key).map_err(status)?;
            listener.udp.schedule(&self.port, key)?;
            listener.tcp.schedule(&self.port, tcp_key)?;
            additions.push(listener);
        }
        self.listeners
            .retain(|listener| settings.listen.contains(&listener.binding));
        self.listeners.extend(additions);
        if self.upstream != settings.upstream {
            self.cache = Cache::new(2048, 2 * 1024 * 1024);
            self.upstream = settings.upstream;
        }
        Ok(())
    }

    pub(super) fn run(mut self) -> Result<(), Status> {
        self.arm_control()?;
        loop {
            let now = monotonic();
            self.expire(now)?;
            let next = self
                .transactions
                .iter()
                .map(|txn| txn.expires.min(txn.attempt.deadline))
                .chain(self.clients.iter().map(|client| client.expires))
                .min()
                .unwrap_or(native_abi::DEADLINE_INFINITE);
            let packet = match self.port.wait(next) {
                Ok(packet) => packet,
                Err(native_abi::ERR_TIMED_OUT) => continue,
                Err(error) => return Err(error),
            };
            match packet.key {
                CONTROL => {
                    if packet.observed & signals::PEER_CLOSED != 0 {
                        return Ok(());
                    }
                    self.dispatch()?;
                    self.arm_control()?;
                }
                NETWORK => return Err(native_abi::ERR_PEER_CLOSED),
                RESUME => self.resume()?,
                key => self.wake_socket(key, packet.kind)?,
            }
        }
    }

    fn arm_control(&self) -> Result<(), Status> {
        self.control.wait_async(
            &self.port,
            CONTROL,
            signals::READABLE | signals::PEER_CLOSED,
        )
    }

    fn dispatch(&mut self) -> Result<(), Status> {
        let message = self.control.read()?;
        let mut handles = message.handles.into_iter();
        let Some(reply) = handles.next().map(Channel::from_handle) else {
            return Ok(());
        };
        let Some((command, body)) = message.bytes.split_first_chunk::<4>() else {
            return Ok(());
        };
        let result = match u32::from_le_bytes(*command) {
            protocol::PAUSE if body.is_empty() => match (handles.next(), handles.next()) {
                (Some(token), None) => self.pause(Channel::from_handle(token)),
                _ => Err(native_abi::ERR_INVALID_ARGS),
            },
            protocol::APPLY if handles.next().is_none() && self.paused() => {
                if body.is_empty() {
                    self.transactions.clear();
                    self.clients.clear();
                    self.listeners.clear();
                    self.upstream.clear();
                    self.cache = Cache::new(2048, 2 * 1024 * 1024);
                    Ok(())
                } else {
                    Settings::decode(body)
                        .map_err(|_| native_abi::ERR_INVALID_ARGS)
                        .and_then(|settings| self.apply(settings))
                }
            }
            _ => Err(native_abi::ERR_INVALID_ARGS),
        };
        let status = result.err().unwrap_or(native_abi::OK);
        let _ = reply.write(&status.to_le_bytes(), Vec::new());
        Ok(())
    }

    fn paused(&self) -> bool {
        self.pause.as_ref().is_some_and(|pause| {
            pause.wait_one(signals::PEER_CLOSED, 0) == Err(native_abi::ERR_TIMED_OUT)
        })
    }

    fn pause(&mut self, token: Channel) -> Result<(), Status> {
        if self.paused() {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        token.wait_async(&self.port, RESUME, signals::PEER_CLOSED)?;
        self.pause = Some(token);
        self.transactions.clear();
        self.clients.clear();
        Ok(())
    }

    fn resume(&mut self) -> Result<(), Status> {
        if self.pause.is_some() && !self.paused() {
            self.pause = None;
            for listener in &mut self.listeners {
                listener.udp.schedule(&self.port, listener.key)?;
                listener.tcp.schedule(&self.port, listener.key + 1)?;
            }
        }
        Ok(())
    }

    fn wake_socket(&mut self, key: u64, kind: u32) -> Result<(), Status> {
        if let Some(index) = self
            .listeners
            .iter()
            .position(|item| item.key == key || item.key + 1 == key)
        {
            if self.listeners[index].key == key {
                self.listeners[index].udp.woke(kind)?;
                if self.pause.is_none() {
                    self.receive(index, key)?;
                }
            } else {
                self.listeners[index].tcp.woke(kind)?;
                if self.pause.is_none() {
                    self.accept(index, key)?;
                }
            }
        } else if let Some(index) = self.clients.iter().position(|client| client.key == key) {
            self.clients[index].socket.woke(kind)?;
            self.drive_client(index)?;
        } else if let Some(index) = self.transactions.iter().position(|txn| txn.key == key) {
            let mut txn = self.transactions.swap_remove(index);
            txn.attempt.socket.woke(kind)?;
            self.drive(txn)?;
        }
        Ok(())
    }

    fn key(&mut self) -> Result<u64, Status> {
        let key = self.next_key;
        self.next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        Ok(key)
    }

    fn accept(&mut self, index: usize, key: u64) -> Result<(), Status> {
        for _ in 0..BATCH {
            let endpoint = match self.listeners[index].tcp.endpoint.accept() {
                Ok(endpoint) => endpoint,
                Err(SocketError::Again) => return self.listeners[index].tcp.arm(&self.port, key),
                Err(SocketError::ConnectionAborted) => continue,
                Err(error) => return Err(status(error)),
            };
            if self.clients.len() == MAX_CLIENTS || self.clients.try_reserve(1).is_err() {
                continue;
            }
            let Ok(socket) = Socket::new(endpoint) else {
                continue;
            };
            let key = self.key()?;
            self.clients.push(Client {
                key,
                socket,
                state: ClientState::Reading,
                input: StreamInput::default(),
                expires: monotonic().saturating_add(CLIENT_IDLE),
            });
            self.drive_client(self.clients.len() - 1)?;
        }
        self.listeners[index].tcp.schedule(&self.port, key)
    }

    fn receive(&mut self, index: usize, key: u64) -> Result<(), Status> {
        for _ in 0..BATCH {
            let received = match self.listeners[index].udp.endpoint.receive(DNS_MAX, false) {
                Ok(received) => received,
                Err(SocketError::Again) => return self.listeners[index].udp.arm(&self.port, key),
                Err(error) => return Err(status(error)),
            };
            if let Some(peer) = received.source
                && received.full_length == received.data.len()
                && !peer.ip().is_unspecified()
                && !peer.ip().is_multicast()
                && peer.port() != 0
            {
                let peer = scoped(peer, self.listeners[index].binding.interface);
                self.begin(
                    Target::Udp {
                        listener: index,
                        peer,
                    },
                    received.data,
                )?;
            }
        }
        self.listeners[index].udp.schedule(&self.port, key)
    }

    fn drive_client(&mut self, index: usize) -> Result<(), Status> {
        let client = &mut self.clients[index];
        if let ClientState::Writing(output) = &mut client.state {
            match output.flush(&client.socket) {
                Ok(true) => client.state = ClientState::Reading,
                Ok(false) => return client.socket.arm(&self.port, client.key),
                Err(_) => {
                    self.close_client(index);
                    return Ok(());
                }
            }
        }
        if !matches!(client.state, ClientState::Reading) {
            return Ok(());
        }
        let request = match client.input.receive(&client.socket) {
            Ok(Some(request)) => request,
            Ok(None) => return client.socket.arm(&self.port, client.key),
            Err(_) => {
                self.close_client(index);
                return Ok(());
            }
        };
        client.state = ClientState::Waiting;
        client.expires = monotonic().saturating_add(CLIENT_IDLE);
        let key = client.key;
        self.begin(Target::Tcp(key), request)
    }

    fn begin(&mut self, target: Target, mut request: Vec<u8>) -> Result<(), Status> {
        let Ok(query) = Query::parse(&request) else {
            if let Target::Tcp(key) = target
                && let Some(index) = self.clients.iter().position(|client| client.key == key)
            {
                self.close_client(index);
            }
            return Ok(());
        };
        if self.output.is_empty() {
            self.output
                .try_reserve_exact(DNS_MAX)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            self.output.resize(DNS_MAX, 0);
        }
        if let Some(length) =
            self.cache
                .answer(query, now_ms(), target.udp_limit(), &mut self.output)
        {
            return self.deliver(target, length);
        }
        if self.transactions.len() == MAX_TRANSACTIONS || self.transactions.try_reserve(1).is_err()
        {
            return self.fail(target, &request);
        }
        let now = monotonic();
        let Some((upstream, attempt)) = self.open_attempt(
            0..self.upstream.len(),
            target.use_tcp(request.len()),
            &mut request,
        ) else {
            return self.fail(target, &request);
        };
        let key = self.key()?;
        self.drive(Transaction {
            key,
            target,
            request,
            upstream,
            attempt,
            expires: now.saturating_add(QUERY_BUDGET),
        })
    }

    fn open_attempt(
        &self,
        mut resolvers: Range<usize>,
        tcp: bool,
        request: &mut [u8],
    ) -> Option<(usize, Attempt)> {
        resolvers.find_map(|index| {
            Attempt::open(&self.network, self.upstream[index], tcp, request)
                .ok()
                .map(|attempt| (index, attempt))
        })
    }

    fn drive(&mut self, mut txn: Transaction) -> Result<(), Status> {
        match txn.attempt.drive(&mut txn.request) {
            Ok(progress @ (Progress::Wait | Progress::Continue)) => {
                if matches!(progress, Progress::Continue) {
                    txn.attempt.socket.schedule(&self.port, txn.key)?;
                } else {
                    txn.attempt.socket.arm(&self.port, txn.key)?;
                }
                self.transactions.push(txn);
                Ok(())
            }
            Ok(Progress::RetryTcp) => self.retry(txn, true),
            Ok(Progress::Complete(response)) => {
                let query = Query::parse(&txn.request).expect("admitted DNS query");
                if let Ok(answer) = query.verify_reply(txn.attempt.id, &response) {
                    // Try a different resolver before returning a transient failure.
                    if query
                        .match_reply(txn.attempt.id, &response)
                        .is_ok_and(|header| header.rcode() == 2)
                        && txn.upstream + 1 < self.upstream.len()
                    {
                        return self.retry(txn, false);
                    }
                    let _ = self.cache.store(answer, now_ms());
                    let length = answer
                        .emit(txn.target.udp_limit(), &mut self.output)
                        .map_err(|_| native_abi::ERR_IO)?;
                    self.deliver(txn.target, length)
                } else {
                    self.retry(txn, false)
                }
            }
            Err(_) => self.retry(txn, false),
        }
    }

    fn retry(&mut self, mut txn: Transaction, tcp: bool) -> Result<(), Status> {
        if monotonic() >= txn.expires {
            return self.fail(txn.target, &txn.request);
        }
        let same = if tcp {
            self.open_attempt(txn.upstream..txn.upstream + 1, true, &mut txn.request)
        } else {
            None
        };
        let next = same.or_else(|| {
            self.open_attempt(
                txn.upstream + 1..self.upstream.len(),
                txn.target.use_tcp(txn.request.len()),
                &mut txn.request,
            )
        });
        let Some((upstream, attempt)) = next else {
            return self.fail(txn.target, &txn.request);
        };
        // Late readiness from a closed socket cannot advance its replacement.
        txn.key = self.key()?;
        txn.upstream = upstream;
        txn.attempt = attempt;
        self.drive(txn)
    }

    fn fail(&mut self, target: Target, request: &[u8]) -> Result<(), Status> {
        let query = Query::parse(request).expect("admitted DNS query");
        let length = query
            .failure(&mut self.output)
            .map_err(|_| native_abi::ERR_IO)?;
        self.deliver(target, length)
    }

    fn deliver(&mut self, target: Target, length: usize) -> Result<(), Status> {
        let bytes = &self.output[..length];
        match target {
            Target::Udp { listener, peer } => {
                let _ = self.listeners[listener].udp.send(bytes, Some(peer));
            }
            Target::Tcp(key) => {
                if let Some(index) = self.clients.iter().position(|client| client.key == key) {
                    match StreamOutput::new(bytes) {
                        Ok(reply) => {
                            let client = &mut self.clients[index];
                            client.state = ClientState::Writing(reply);
                            client.expires = monotonic().saturating_add(CLIENT_IDLE);
                            // Don't nest delivery with the next request from a pipelining client.
                            client.socket.schedule(&self.port, client.key)?;
                        }
                        Err(_) => {
                            self.close_client(index);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn close_client(&mut self, index: usize) {
        let client = self.clients.swap_remove(index);
        if matches!(client.state, ClientState::Waiting)
            && let Some(transaction) = self
                .transactions
                .iter()
                .position(|txn| matches!(txn.target, Target::Tcp(key) if key == client.key))
        {
            self.transactions.swap_remove(transaction);
        }
    }

    fn expire(&mut self, now: i64) -> Result<(), Status> {
        let mut index = 0;
        while index < self.clients.len() {
            if self.clients[index].expires <= now {
                self.close_client(index);
            } else {
                index += 1;
            }
        }
        let mut index = 0;
        while index < self.transactions.len() {
            let txn = &self.transactions[index];
            if txn.expires <= now || txn.attempt.deadline <= now {
                let txn = self.transactions.swap_remove(index);
                self.retry(txn, false)?;
            } else {
                index += 1;
            }
        }
        Ok(())
    }
}

fn scoped(address: InetAddress, interface: u32) -> InetAddress {
    if let InetAddress::V6(address) = address
        && address.ip().is_unicast_link_local()
        && address.scope_id() == 0
    {
        return SocketAddrV6::new(*address.ip(), address.port(), 0, interface).into();
    }
    address
}

fn now_ms() -> u64 {
    monotonic().max(0) as u64 / 1_000_000
}

pub(super) fn status(error: SocketError) -> Status {
    match error {
        SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        SocketError::AddressInUse => native_abi::ERR_ALREADY_BOUND,
        SocketError::NoDevice | SocketError::AddressNotAvailable => native_abi::ERR_NOT_FOUND,
        SocketError::NetworkDown | SocketError::BrokenPipe => native_abi::ERR_PEER_CLOSED,
        _ => native_abi::ERR_IO,
    }
}
