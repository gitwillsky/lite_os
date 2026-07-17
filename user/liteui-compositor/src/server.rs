use alloc::vec::Vec;
use core::ptr;

use liteui_core::{
    Anchors, GridCell, GridUpdate, Mutation, NodeId, NodeRole, Rect as UiRect, Style, TextRun,
};

mod events;
mod frame;
mod peer;

use frame::{FrameKind, Header, decode_grid_cell};
use peer::Peer;

use crate::{
    ffi,
    scene::{Damage, GridConfiguration, Scene, TEXT_GRID_CAPACITY, TerminalPointer},
};

const SOCKET_PATH: &[u8] = b"/run/liteui/compositor.sock\0";
const CLIENT_COUNT: usize = 3;
const CLIENT_UIDS: [u32; CLIENT_COUNT] = [100, 101, 102];
const CLIENT_NODE_CAPACITY: usize = 256;
const HEADER_BYTES: usize = 40;
const OPERATION_BYTES: usize = 40;
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_OPERATIONS: usize = 256;
const MAX_FRAME_BYTES: usize = HEADER_BYTES + MAX_PAYLOAD_BYTES;
const GRID_CELL_BYTES: usize = 16;
const TERMINAL_SLOT: usize = 1;

pub struct Server {
    listener: i32,
    clients: [Option<Client>; CLIENT_COUNT],
    generations: [u16; CLIENT_COUNT],
}

struct Client {
    fd: i32,
    slot: ClientSlot,
    generation: u16,
    bytes: Vec<u8>,
    mutations: Vec<Mutation>,
    cells: Vec<GridCell>,
    next_sequence: u64,
    tree_published: bool,
    grid_published: bool,
    events: events::Queue,
    grid_configuration: Option<GridConfiguration>,
}

#[derive(Clone, Copy)]
pub struct ClientSlot(usize);

impl ClientSlot {
    pub const fn index(self) -> usize {
        self.0
    }

    pub fn root(self, generation: u16) -> NodeId {
        NodeId::new((2 + self.0 * CLIENT_NODE_CAPACITY) as u16, generation)
    }

    pub const fn is_terminal(self) -> bool {
        self.0 == TERMINAL_SLOT
    }
}

impl Server {
    pub fn open() -> Result<Self, ()> {
        let listener = service_activation::take_listener(b"liteui-compositor")?;
        Ok(Self {
            listener,
            clients: [None, None, None],
            generations: [0; CLIENT_COUNT],
        })
    }

    pub fn listener_fd(&self) -> i32 {
        self.listener
    }

    pub fn client_fds(&self) -> [i32; CLIENT_COUNT] {
        core::array::from_fn(|index| self.clients[index].as_ref().map_or(-1, |client| client.fd))
    }

    pub fn client_events(&self) -> [i16; CLIENT_COUNT] {
        core::array::from_fn(|index| {
            self.clients[index].as_ref().map_or(ffi::POLLIN, |client| {
                ffi::POLLIN
                    | if client.events.is_empty() {
                        0
                    } else {
                        ffi::POLLOUT
                    }
            })
        })
    }

    pub fn client_mask(&self) -> u32 {
        self.clients
            .iter()
            .enumerate()
            .fold(0u32, |mask, (index, client)| {
                mask | u32::from(client.is_some()) << index
            })
    }

    pub fn flush(&mut self, slot: ClientSlot) -> Result<(), ()> {
        self.clients[slot.index()].as_mut().ok_or(())?.flush()
    }

    pub fn terminal_accepts_key_batch(&self) -> bool {
        self.clients[TERMINAL_SLOT]
            .as_ref()
            // One pointer action may be returned by the same poll snapshot after the
            // maximum 32-event keyboard read. Reserving it prevents imported input loss.
            .is_some_and(|client| client.events.remaining() > 32)
    }

    pub fn terminal_accepts_pointer(&self) -> bool {
        self.clients[TERMINAL_SLOT]
            .as_ref()
            .is_some_and(|client| client.events.remaining() != 0)
    }

    pub fn queue_click(&mut self, node: NodeId) -> Result<bool, ()> {
        let (slot, local_index) = local_identity(node)?;
        let Some(client) = self.clients[slot.index()].as_mut() else {
            return Ok(false);
        };
        if !client.tree_published {
            return Ok(false);
        }
        let local_generation = node
            .generation()
            .checked_sub(client.generation.saturating_sub(1))
            .filter(|generation| *generation != 0)
            .ok_or(())?;
        let mut payload = [0u8; 8];
        payload[..2].copy_from_slice(&local_index.to_le_bytes());
        payload[2..4].copy_from_slice(&local_generation.to_le_bytes());
        client.events.push(1, payload)
    }

    pub fn queue_key(&mut self, code: u16, value: i32) -> Result<bool, ()> {
        let Some(client) = self.clients[TERMINAL_SLOT].as_mut() else {
            return Ok(false);
        };
        let mut payload = [0u8; 8];
        payload[..2].copy_from_slice(&code.to_le_bytes());
        payload[2..6].copy_from_slice(&value.to_le_bytes());
        client.events.push(2, payload)
    }

    pub fn queue_grid_configuration(
        &mut self,
        configuration: GridConfiguration,
    ) -> Result<bool, ()> {
        let Some(client) = self.clients[TERMINAL_SLOT].as_mut() else {
            return Ok(false);
        };
        if client.grid_configuration == Some(configuration) {
            return Ok(true);
        }
        let mut payload = [0u8; 8];
        payload[..2].copy_from_slice(&configuration.columns.to_le_bytes());
        payload[2..4].copy_from_slice(&configuration.rows.to_le_bytes());
        payload[4..6].copy_from_slice(&configuration.pixel_width.to_le_bytes());
        payload[6..8].copy_from_slice(&configuration.pixel_height.to_le_bytes());
        if client.events.push(3, payload)? {
            client.grid_configuration = Some(configuration);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn queue_terminal_pointer(&mut self, event: TerminalPointer) -> Result<bool, ()> {
        let Some(client) = self.clients[TERMINAL_SLOT].as_mut() else {
            return Ok(false);
        };
        let mut payload = [0u8; 8];
        payload[0] = event.button;
        payload[1] = u8::from(event.pressed);
        payload[2..4].copy_from_slice(&event.column.to_le_bytes());
        payload[4..6].copy_from_slice(&event.row.to_le_bytes());
        client.events.push(5, payload)
    }

    pub fn accept(&mut self, diagnostics: &[u8]) -> Result<(), ()> {
        loop {
            let fd = unsafe {
                ffi::accept4(
                    self.listener,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ffi::SOCK_NONBLOCK | ffi::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                return match ffi::errno() {
                    ffi::EAGAIN => Ok(()),
                    ffi::EINTR => continue,
                    _ => Err(()),
                };
            }
            let Some(peer) = peer::classify(fd) else {
                unsafe { ffi::close(fd) };
                continue;
            };
            let Peer::Client(slot) = peer else {
                peer::send_snapshot(fd, diagnostics);
                continue;
            };
            if self.clients[slot.index()].is_some() {
                unsafe { ffi::close(fd) };
                continue;
            }
            let Some(generation) = self.generations[slot.index()].checked_add(1) else {
                unsafe { ffi::close(fd) };
                continue;
            };
            self.clients[slot.index()] = match Client::try_new(fd, slot, generation) {
                Ok(client) => {
                    self.generations[slot.index()] = generation;
                    Some(client)
                }
                Err(()) => {
                    unsafe { ffi::close(fd) };
                    return Err(());
                }
            };
        }
    }

    pub fn read(&mut self, slot: ClientSlot, scene: &mut Scene) -> Result<Damage, ()> {
        self.clients[slot.index()].as_mut().ok_or(())?.read(scene)
    }

    pub fn disconnect(&mut self, slot: ClientSlot, scene: &mut Scene) -> Result<Damage, ()> {
        let mut identity = None;
        let mut damage = Damage::EMPTY;
        if let Some(client) = self.clients[slot.index()].take() {
            identity = client
                .tree_published
                .then_some((client.generation, client.slot));
            if client.grid_published {
                damage.merge(scene.deactivate_grid());
            }
            unsafe { ffi::close(client.fd) };
        }
        damage.merge(scene.deactivate_client(identity)?);
        Ok(damage)
    }

    pub const fn slot(index: usize) -> ClientSlot {
        ClientSlot(index)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for client in &mut self.clients {
            if let Some(client) = client.take() {
                unsafe { ffi::close(client.fd) };
            }
        }
        unsafe { ffi::close(self.listener) };
        unsafe { ffi::unlink(SOCKET_PATH.as_ptr().cast()) };
    }
}

impl Client {
    fn try_new(fd: i32, slot: ClientSlot, generation: u16) -> Result<Self, ()> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(MAX_FRAME_BYTES).map_err(|_| ())?;
        let mut mutations = Vec::new();
        mutations
            .try_reserve_exact(MAX_OPERATIONS)
            .map_err(|_| ())?;
        let mut cells = Vec::new();
        if slot.is_terminal() {
            cells
                .try_reserve_exact(TEXT_GRID_CAPACITY)
                .map_err(|_| ())?;
        }
        Ok(Self {
            fd,
            slot,
            generation,
            bytes,
            mutations,
            cells,
            next_sequence: 1,
            tree_published: false,
            grid_published: false,
            events: events::Queue::new(),
            grid_configuration: None,
        })
    }

    fn read(&mut self, scene: &mut Scene) -> Result<Damage, ()> {
        let mut damage = Damage::EMPTY;
        loop {
            if self.bytes.len() == MAX_FRAME_BYTES {
                return Err(());
            }
            let available = MAX_FRAME_BYTES - self.bytes.len();
            let count = unsafe {
                ffi::read(
                    self.fd,
                    self.bytes.as_mut_ptr().add(self.bytes.len()).cast(),
                    available,
                )
            };
            if count > 0 {
                let length = self.bytes.len().checked_add(count as usize).ok_or(())?;
                unsafe { self.bytes.set_len(length) };
                self.publish_complete(scene, &mut damage)?;
            } else if count < 0 && ffi::errno() == ffi::EINTR {
                continue;
            } else if count < 0 && ffi::errno() == ffi::EAGAIN {
                return Ok(damage);
            } else {
                return Err(());
            }
        }
    }

    fn publish_complete(&mut self, scene: &mut Scene, damage: &mut Damage) -> Result<(), ()> {
        loop {
            let Some(header) = Header::decode(&self.bytes)? else {
                return Ok(());
            };
            if header.epoch != 1 || header.sequence != self.next_sequence {
                return Err(());
            }
            let frame_length = HEADER_BYTES.checked_add(header.payload_length).ok_or(())?;
            if self.bytes.len() < frame_length {
                return Ok(());
            }
            match header.kind {
                FrameKind::Ui { .. } if !self.slot.is_terminal() => {
                    self.mutations.clear();
                    for (index, operation) in self.bytes[HEADER_BYTES..frame_length]
                        .chunks_exact(OPERATION_BYTES)
                        .enumerate()
                    {
                        let decoded = decode_operation(operation)?;
                        self.mutations.push(remap_operation(
                            decoded,
                            self.slot,
                            self.generation,
                            self.tree_published,
                            index,
                        )?);
                    }
                    damage.merge(scene.publish_client(self.slot, &self.mutations)?);
                    self.tree_published = true;
                }
                FrameKind::Grid(grid) if self.slot.is_terminal() => {
                    self.cells.clear();
                    for cell in self.bytes[HEADER_BYTES..frame_length].chunks_exact(GRID_CELL_BYTES)
                    {
                        self.cells.push(decode_grid_cell(cell)?);
                    }
                    damage.merge(
                        scene.publish_grid(GridUpdate {
                            epoch: header.epoch,
                            sequence: header.sequence,
                            columns: usize::from(grid.columns),
                            rows: usize::from(grid.rows),
                            cursor: grid
                                .cursor
                                .map(|(row, column)| (usize::from(row), usize::from(column))),
                            reverse: grid.reverse,
                            blink_visible: grid.blink_visible,
                            cells: &self.cells,
                        })?,
                    );
                    self.grid_published = true;
                }
                _ => return Err(()),
            }
            self.next_sequence = self.next_sequence.checked_add(1).ok_or(())?;
            self.bytes.copy_within(frame_length.., 0);
            self.bytes.truncate(self.bytes.len() - frame_length);
        }
    }

    fn flush(&mut self) -> Result<(), ()> {
        self.events.flush(self.fd)
    }
}

fn decode_operation(bytes: &[u8]) -> Result<Mutation, ()> {
    if bytes.len() != OPERATION_BYTES {
        return Err(());
    }
    if bytes[0] == 4 {
        return decode_text(bytes);
    }
    if bytes[1] & !1 != 0 || bytes[10..12] != [0, 0] || bytes[39] != 0 {
        return Err(());
    }
    let id = NodeId::new(read_u16(bytes, 2)?, read_u16(bytes, 4)?);
    let parent = NodeId::new(read_u16(bytes, 6)?, read_u16(bytes, 8)?);
    let style = Style {
        bounds: UiRect::from_pixels(
            read_i32(bytes, 12)?,
            read_i32(bytes, 16)?,
            read_i32(bytes, 20)?,
            read_i32(bytes, 24)?,
        ),
        background: read_u32(bytes, 28)?,
        border_color: read_u32(bytes, 32)?,
        border_width: bytes[36],
        visible: bytes[1] & 1 != 0,
        anchors: Anchors::from_bits(bytes[37]).ok_or(())?,
        role: NodeRole::from_u8(bytes[38]).ok_or(())?,
    };
    match bytes[0] {
        1 => Ok(Mutation::Create { id, parent, style }),
        2 if parent == NodeId::new(0, 0) => Ok(Mutation::SetStyle { id, style }),
        3 if bytes[1] == 0
            && parent == NodeId::new(0, 0)
            && bytes[12..].iter().all(|byte| *byte == 0) =>
        {
            Ok(Mutation::Remove { id })
        }
        _ => Err(()),
    }
}

fn decode_text(bytes: &[u8]) -> Result<Mutation, ()> {
    let length = usize::from(bytes[6]);
    if bytes[1] & !1 != 0
        || bytes[7] != 0
        || length == 0
        || length > 24
        || bytes[12 + length..36].iter().any(|byte| *byte != 0)
        || bytes[36..40] != [0, 0, 0, 0]
    {
        return Err(());
    }
    let text =
        TextRun::try_new(&bytes[12..12 + length], read_u32(bytes, 8)?, bytes[1] != 0).ok_or(())?;
    Ok(Mutation::SetText {
        id: NodeId::new(read_u16(bytes, 2)?, read_u16(bytes, 4)?),
        text,
    })
}

fn remap_operation(
    mutation: Mutation,
    slot: ClientSlot,
    connection_generation: u16,
    published: bool,
    operation_index: usize,
) -> Result<Mutation, ()> {
    if !published && operation_index == 0 {
        let Mutation::SetStyle { id, style } = mutation else {
            return Err(());
        };
        if id != NodeId::ROOT {
            return Err(());
        }
        return Ok(Mutation::Create {
            id: slot.root(connection_generation),
            parent: NodeId::ROOT,
            style: layer_style(style),
        });
    }
    match mutation {
        Mutation::Create { id, parent, style } if id != NodeId::ROOT => Ok(Mutation::Create {
            id: mapped(slot, connection_generation, id)?,
            parent: mapped(slot, connection_generation, parent)?,
            style,
        }),
        Mutation::SetStyle { id, style } => Ok(Mutation::SetStyle {
            id: mapped(slot, connection_generation, id)?,
            style: if id == NodeId::ROOT {
                layer_style(style)
            } else {
                style
            },
        }),
        Mutation::SetText { id, text } => Ok(Mutation::SetText {
            id: mapped(slot, connection_generation, id)?,
            text,
        }),
        Mutation::Remove { id } if id != NodeId::ROOT => Ok(Mutation::Remove {
            id: mapped(slot, connection_generation, id)?,
        }),
        _ => Err(()),
    }
}

fn mapped(slot: ClientSlot, connection_generation: u16, local: NodeId) -> Result<NodeId, ()> {
    if local.index() == 0 || local.index() >= CLIENT_NODE_CAPACITY || local.generation() == 0 {
        return Err(());
    }
    let index = local
        .index()
        .checked_add(1 + slot.index() * CLIENT_NODE_CAPACITY)
        .and_then(|index| u16::try_from(index).ok())
        .ok_or(())?;
    let generation = local
        .generation()
        .checked_add(connection_generation.saturating_sub(1))
        .ok_or(())?;
    Ok(NodeId::new(index, generation))
}

fn local_identity(global: NodeId) -> Result<(ClientSlot, u16), ()> {
    let shifted = global.index().checked_sub(2).ok_or(())?;
    let slot = shifted / CLIENT_NODE_CAPACITY;
    if slot >= CLIENT_COUNT {
        return Err(());
    }
    let local = shifted % CLIENT_NODE_CAPACITY + 1;
    Ok((ClientSlot(slot), u16::try_from(local).map_err(|_| ())?))
}

fn layer_style(style: Style) -> Style {
    Style {
        bounds: UiRect::from_pixels(0, 0, 0, 0),
        anchors: Anchors::from_bits(Anchors::STRETCH_WIDTH | Anchors::STRETCH_HEIGHT)
            .unwrap_or(Anchors::NONE),
        role: NodeRole::Normal,
        ..style
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ()> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ()> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, ()> {
    Ok(i32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}
