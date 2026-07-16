mod raster;
mod recovery;
mod text_grid;

use liteui_core::{DrawList, Mutation, Scene as UiScene, TextGrid, Transaction};

use crate::font::Atlas;
use crate::server::ClientSlot;
use crate::window::WindowManager;
use recovery::{TASKBAR_HEIGHT, TITLE_HEIGHT};
pub use text_grid::{GridConfiguration, TEXT_GRID_CAPACITY, TerminalPointer};

const MAX_DAMAGE_RECTS: usize = 4;
const POINTER_WIDTH: usize = 18;
const POINTER_HEIGHT: usize = 24;
const UI_NODE_CAPACITY: usize = 16;
const CLIENT_NODE_CAPACITY: usize = 256;
const CLIENT_COUNT: usize = 3;
const CLIENT_SCENE_CAPACITY: usize = 1 + CLIENT_NODE_CAPACITY * CLIENT_COUNT;
const CLIENT_DRAW_CAPACITY: usize = CLIENT_SCENE_CAPACITY * 2;
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x1: usize,
    pub y1: usize,
    pub x2: usize,
    pub y2: usize,
}

impl Rect {
    pub fn full(width: usize, height: usize) -> Self {
        Self {
            x1: 0,
            y1: 0,
            x2: width,
            y2: height,
        }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
            x2: self.x2.max(other.x2),
            y2: self.y2.max(other.y2),
        }
    }

    pub(super) fn from_ui(rectangle: liteui_core::Rect) -> Self {
        let x = rectangle.x.floor_pixels().max(0) as usize;
        let y = rectangle.y.floor_pixels().max(0) as usize;
        Self {
            x1: x,
            y1: y,
            x2: rectangle
                .x
                .floor_pixels()
                .saturating_add(rectangle.width.ceil_pixels())
                .max(0) as usize,
            y2: rectangle
                .y
                .floor_pixels()
                .saturating_add(rectangle.height.ceil_pixels())
                .max(0) as usize,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Damage {
    rectangles: [Rect; MAX_DAMAGE_RECTS],
    count: usize,
}

impl Damage {
    pub const EMPTY: Self = Self {
        rectangles: [Rect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        }; MAX_DAMAGE_RECTS],
        count: 0,
    };

    pub fn rectangles(&self) -> &[Rect] {
        &self.rectangles[..self.count]
    }

    pub fn push(&mut self, rectangle: Rect) {
        if rectangle.x1 >= rectangle.x2 || rectangle.y1 >= rectangle.y2 {
            return;
        }
        if self.count < MAX_DAMAGE_RECTS {
            self.rectangles[self.count] = rectangle;
            self.count += 1;
            return;
        }
        let mut merged = rectangle;
        for current in &self.rectangles {
            merged = merged.union(*current);
        }
        self.rectangles[0] = merged;
        self.count = 1;
    }

    pub fn merge(&mut self, other: Self) {
        for rectangle in other.rectangles().iter().copied() {
            self.push(rectangle);
        }
    }

    pub(super) fn one(rectangle: Rect) -> Self {
        let mut damage = Self::EMPTY;
        damage.push(rectangle);
        damage
    }

    pub(super) fn pair(first: Rect, second: Rect) -> Self {
        let mut damage = Self::one(first);
        damage.push(second);
        damage
    }
}

#[derive(Clone, Copy)]
pub(super) struct Size {
    width: usize,
    height: usize,
}

#[derive(Clone, Copy)]
pub(super) struct Window {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Window {
    fn rectangle(self) -> Rect {
        Rect {
            x1: self.x,
            y1: self.y,
            x2: self.x.saturating_add(self.width),
            y2: self.y.saturating_add(self.height),
        }
    }
}

#[derive(Clone, Copy)]
struct Point {
    x: usize,
    y: usize,
}

#[derive(Clone, Copy)]
enum PointerMode {
    Idle,
    Dragging { offset_x: usize, offset_y: usize },
}

#[derive(Clone, Copy)]
struct InitialState {
    viewport: Size,
    window: Window,
    pointer: Point,
    focused: bool,
    accent: usize,
}

/// Compositor-owned recovery scene and window interaction state.
///
/// The recovery nodes use the same `liteui-core` transaction seam as future
/// QuickJS clients. They remain visible only until System Shell atomically
/// publishes its first root.
pub struct Scene {
    viewport: Size,
    window: Window,
    pointer: Point,
    pointer_mode: PointerMode,
    focused: bool,
    accent: usize,
    sequence: u64,
    ui: UiScene,
    draw_list: DrawList,
    client_ui: UiScene,
    client_draw_list: DrawList,
    active_clients: [bool; CLIENT_COUNT],
    client_sequence: u64,
    text_grid: TextGrid,
    font: Atlas,
    windows: WindowManager,
}

impl Scene {
    pub fn try_new(width: usize, height: usize) -> Result<Self, ()> {
        let viewport = Size { width, height };
        let window_size = recovery::window_size(viewport);
        let workspace_height = height.saturating_sub(TASKBAR_HEIGHT);
        Self::try_with_state(
            InitialState {
                viewport,
                window: Window {
                    x: width.saturating_sub(window_size.width) / 2,
                    y: workspace_height.saturating_sub(window_size.height) / 2,
                    width: window_size.width,
                    height: window_size.height,
                },
                pointer: Point {
                    x: width / 2,
                    y: height / 2,
                },
                focused: true,
                accent: 0,
            },
            None,
            None,
            None,
            [false; CLIENT_COUNT],
            1,
        )
    }

    pub fn try_resized(&self, width: usize, height: usize) -> Result<Self, ()> {
        let viewport = Size { width, height };
        let size = recovery::window_size(viewport);
        Self::try_with_state(
            InitialState {
                viewport,
                window: Window {
                    x: self.window.x.min(width.saturating_sub(size.width)),
                    y: self
                        .window
                        .y
                        .min(height.saturating_sub(TASKBAR_HEIGHT + size.height)),
                    width: size.width,
                    height: size.height,
                },
                pointer: Point {
                    x: self.pointer.x.min(width.saturating_sub(1)),
                    y: self.pointer.y.min(height.saturating_sub(1)),
                },
                focused: self.focused,
                accent: self.accent,
            },
            Some(&self.client_ui),
            Some(&self.windows),
            Some(&self.text_grid),
            self.active_clients,
            self.client_sequence,
        )
    }

    fn try_with_state(
        state: InitialState,
        previous_client: Option<&UiScene>,
        previous_windows: Option<&WindowManager>,
        previous_grid: Option<&TextGrid>,
        active_clients: [bool; CLIENT_COUNT],
        client_sequence: u64,
    ) -> Result<Self, ()> {
        let mut ui = UiScene::try_new(1, UI_NODE_CAPACITY, recovery::root_style(state.viewport))
            .map_err(|_| ())?;
        let mutations =
            recovery::create_nodes(state.viewport, state.window, state.focused, state.accent);
        ui.commit(Transaction {
            session_epoch: 1,
            sequence: 1,
            mutations: &mutations,
        })
        .map_err(|_| ())?;
        let mut draw_list = DrawList::try_with_capacity(UI_NODE_CAPACITY).map_err(|_| ())?;
        ui.build_draw_list(&mut draw_list).map_err(|_| ())?;
        let client_bounds = recovery::root_style(state.viewport).bounds;
        let client_ui = previous_client
            .map_or_else(
                || UiScene::try_new(1, CLIENT_SCENE_CAPACITY, client_root_style(state.viewport)),
                |client| client.try_clone_resized(client_bounds),
            )
            .map_err(|_| ())?;
        let mut client_draw_list =
            DrawList::try_with_capacity(CLIENT_DRAW_CAPACITY).map_err(|_| ())?;
        if active_clients.iter().any(|active| *active) {
            client_ui
                .build_draw_list(&mut client_draw_list)
                .map_err(|_| ())?;
        }
        let mut windows = previous_windows.map_or_else(
            || WindowManager::new(state.viewport.width, state.viewport.height),
            |manager| manager.resized(state.viewport.width, state.viewport.height),
        );
        if active_clients.iter().any(|active| *active) {
            windows.reconcile(client_draw_list.as_slice());
        }
        let text_grid = previous_grid
            .map_or_else(
                || TextGrid::try_new(1, TEXT_GRID_CAPACITY),
                TextGrid::try_clone,
            )
            .map_err(|_| ())?;
        Ok(Self {
            viewport: state.viewport,
            window: state.window,
            pointer: state.pointer,
            pointer_mode: PointerMode::Idle,
            focused: state.focused,
            accent: state.accent,
            sequence: 2,
            ui,
            draw_list,
            client_ui,
            client_draw_list,
            active_clients,
            client_sequence,
            text_grid,
            font: Atlas::checked().ok_or(())?,
            windows,
        })
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.viewport.width, self.viewport.height)
    }

    pub fn move_window(&mut self, dx: isize, dy: isize) -> Result<Damage, ()> {
        if self.has_clients() {
            return Ok(self.windows.move_focused(dx, dy));
        }
        let old = self.window_damage();
        let next = Window {
            x: shifted(
                self.window.x,
                dx,
                self.viewport.width.saturating_sub(self.window.width),
            ),
            y: shifted(
                self.window.y,
                dy,
                self.viewport
                    .height
                    .saturating_sub(TASKBAR_HEIGHT + self.window.height),
            ),
            ..self.window
        };
        self.commit(&recovery::geometry_mutations(
            next,
            self.focused,
            self.accent,
        ))?;
        self.window = next;
        Ok(Damage::pair(old, self.window_damage()))
    }

    pub fn cycle_accent(&mut self) -> Result<Damage, ()> {
        if self.has_clients() {
            return Ok(Damage::EMPTY);
        }
        let accent = recovery::next_accent(self.accent);
        self.commit(&recovery::chrome_mutations(
            self.window,
            self.focused,
            accent,
        ))?;
        self.accent = accent;
        Ok(Damage::one(self.window_rect()))
    }

    pub fn move_pointer(&mut self, x: usize, y: usize) -> Result<(Damage, bool), ()> {
        let old_pointer = self.pointer_rect();
        let old_window = self.window_damage();
        let pointer = Point {
            x: x.min(self.viewport.width.saturating_sub(1)),
            y: y.min(self.viewport.height.saturating_sub(1)),
        };
        if self.has_clients() {
            self.pointer = pointer;
            let update = self.windows.pointer_moved(pointer.x, pointer.y);
            let mut damage = Damage::pair(old_pointer, self.pointer_rect());
            damage.merge(update.0);
            return Ok((damage, update.1));
        }
        let mut next_window = self.window;
        if let PointerMode::Dragging { offset_x, offset_y } = self.pointer_mode {
            next_window.x = pointer
                .x
                .saturating_sub(offset_x)
                .min(self.viewport.width.saturating_sub(self.window.width));
            next_window.y = pointer.y.saturating_sub(offset_y).min(
                self.viewport
                    .height
                    .saturating_sub(TASKBAR_HEIGHT + self.window.height),
            );
        }
        let geometry = next_window.x != self.window.x || next_window.y != self.window.y;
        if geometry {
            self.commit(&recovery::geometry_mutations(
                next_window,
                self.focused,
                self.accent,
            ))?;
            self.window = next_window;
        }
        self.pointer = pointer;
        let mut damage = Damage::pair(old_pointer, self.pointer_rect());
        if geometry {
            damage.push(old_window.union(self.window_damage()));
        }
        Ok((damage, geometry))
    }

    pub fn set_primary_button(
        &mut self,
        pressed: bool,
    ) -> Result<(Damage, bool, Option<liteui_core::NodeId>), ()> {
        if self.has_clients() {
            return Ok(self.windows.primary_button(
                self.pointer.x,
                self.pointer.y,
                pressed,
                self.client_draw_list.as_slice(),
            ));
        }
        if !pressed {
            self.pointer_mode = PointerMode::Idle;
            return Ok((Damage::EMPTY, false, None));
        }
        let focused = contains(self.window_rect(), self.pointer.x, self.pointer.y);
        let mode = if focused && contains(self.title_rect(), self.pointer.x, self.pointer.y) {
            PointerMode::Dragging {
                offset_x: self.pointer.x.saturating_sub(self.window.x),
                offset_y: self.pointer.y.saturating_sub(self.window.y),
            }
        } else {
            PointerMode::Idle
        };
        if focused == self.focused {
            self.pointer_mode = mode;
            return Ok((Damage::EMPTY, false, None));
        }
        self.commit(&recovery::chrome_mutations(
            self.window,
            focused,
            self.accent,
        ))?;
        self.focused = focused;
        self.pointer_mode = mode;
        Ok((Damage::one(self.window_rect()), true, None))
    }

    pub fn render(&self, pixels: *mut u32, pitch: usize, rectangle: Rect) {
        raster::render_scene(
            pixels,
            pitch,
            self.viewport.width,
            self.viewport.height,
            rectangle,
            self.draw_list.as_slice(),
            self.font,
            &self.windows,
            None,
            self.pointer_rect(),
        );
        if self.has_clients() {
            raster::render_scene(
                pixels,
                pitch,
                self.viewport.width,
                self.viewport.height,
                rectangle,
                self.client_draw_list.as_slice(),
                self.font,
                &self.windows,
                self.text_grid.snapshot(),
                self.pointer_rect(),
            );
        }
    }

    pub fn publish_client(
        &mut self,
        slot: ClientSlot,
        mutations: &[Mutation],
    ) -> Result<Damage, ()> {
        let next_sequence = self.client_sequence.checked_add(1).ok_or(())?;
        self.client_ui
            .commit(Transaction {
                session_epoch: 1,
                sequence: self.client_sequence,
                mutations,
            })
            .map_err(|_| ())?;
        self.client_sequence = next_sequence;
        self.client_ui
            .build_draw_list(&mut self.client_draw_list)
            .map_err(|_| ())?;
        self.windows.reconcile(self.client_draw_list.as_slice());
        self.active_clients[slot.index()] = true;
        Ok(Damage::one(Rect::full(
            self.viewport.width,
            self.viewport.height,
        )))
    }

    pub fn deactivate_client(&mut self, identity: Option<(u16, ClientSlot)>) -> Result<Damage, ()> {
        let Some((generation, slot)) = identity else {
            return Ok(Damage::EMPTY);
        };
        let next_sequence = self.client_sequence.checked_add(1).ok_or(())?;
        self.client_ui
            .commit(Transaction {
                session_epoch: 1,
                sequence: self.client_sequence,
                mutations: &[Mutation::Remove {
                    id: slot.root(generation),
                }],
            })
            .map_err(|_| ())?;
        self.client_sequence = next_sequence;
        self.active_clients[slot.index()] = false;
        self.client_ui
            .build_draw_list(&mut self.client_draw_list)
            .map_err(|_| ())?;
        self.windows.reconcile(self.client_draw_list.as_slice());
        Ok(Damage::one(Rect::full(
            self.viewport.width,
            self.viewport.height,
        )))
    }

    fn has_clients(&self) -> bool {
        self.active_clients.iter().any(|active| *active)
    }

    fn commit(&mut self, mutations: &[Mutation]) -> Result<(), ()> {
        let next_sequence = self.sequence.checked_add(1).ok_or(())?;
        self.ui
            .commit(Transaction {
                session_epoch: 1,
                sequence: self.sequence,
                mutations,
            })
            .map_err(|_| ())?;
        // liteui-core 已经发布了新状态，sequence 必须立即跟进；缺失该顺序会在
        // 后续 DrawList 不变量失效时把下一次合法事务误判为乱序。
        self.sequence = next_sequence;
        self.ui.build_draw_list(&mut self.draw_list).map_err(|_| ())
    }

    fn window_rect(&self) -> Rect {
        let mut rectangle = self.window.rectangle();
        rectangle.x2 = rectangle.x2.min(self.viewport.width);
        rectangle.y2 = rectangle.y2.min(self.viewport.height);
        rectangle
    }

    fn title_rect(&self) -> Rect {
        let window = self.window_rect();
        Rect {
            y2: (window.y1 + TITLE_HEIGHT).min(window.y2),
            ..window
        }
    }

    fn window_damage(&self) -> Rect {
        let window = self.window_rect();
        Rect {
            x1: window.x1.saturating_sub(recovery::SHADOW),
            y1: window.y1.saturating_sub(recovery::SHADOW),
            x2: (window.x2 + recovery::SHADOW).min(self.viewport.width),
            y2: (window.y2 + recovery::SHADOW).min(self.viewport.height),
        }
    }

    fn pointer_rect(&self) -> Rect {
        Rect {
            x1: self.pointer.x,
            y1: self.pointer.y,
            x2: (self.pointer.x + POINTER_WIDTH).min(self.viewport.width),
            y2: (self.pointer.y + POINTER_HEIGHT).min(self.viewport.height),
        }
    }
}

fn shifted(value: usize, delta: isize, maximum: usize) -> usize {
    value.saturating_add_signed(delta).min(maximum)
}

fn client_root_style(viewport: Size) -> liteui_core::Style {
    liteui_core::Style {
        background: 0,
        visible: false,
        ..recovery::root_style(viewport)
    }
}

fn contains(rectangle: Rect, x: usize, y: usize) -> bool {
    x >= rectangle.x1 && x < rectangle.x2 && y >= rectangle.y1 && y < rectangle.y2
}
