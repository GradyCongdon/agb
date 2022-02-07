use core::cell::RefCell;
use core::ops::{Deref, DerefMut};

use alloc::vec::Vec;
use alloc::{boxed::Box, vec};
use hashbrown::HashMap;

use crate::bitarray::Bitarray;
use crate::{
    display,
    fixnum::{Rect, Vector2D},
    memory_mapped::{MemoryMapped, MemoryMapped1DArray},
};

use super::{
    palette16, set_graphics_mode, set_graphics_settings, DisplayMode, GraphicsSettings, Priority,
    DISPLAY_CONTROL,
};

const TILE_BACKGROUND: MemoryMapped1DArray<u32, { 2048 * 8 }> =
    unsafe { MemoryMapped1DArray::new(0x06000000) };

const PALETTE_BACKGROUND: MemoryMapped1DArray<u16, 256> =
    unsafe { MemoryMapped1DArray::new(0x0500_0000) };

#[derive(Clone, Copy, Debug)]
pub enum TileFormat {
    FourBpp,
}

impl TileFormat {
    /// Returns the size of the tile in bytes
    fn tile_size(self) -> usize {
        match self {
            TileFormat::FourBpp => 8 * 8 / 2,
        }
    }
}

pub struct TileSet<'a> {
    tiles: &'a [u32],
    format: TileFormat,
}

impl<'a> TileSet<'a> {
    pub fn new(tiles: &'a [u32], format: TileFormat) -> Self {
        Self { tiles, format }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TileSetReference {
    id: u16,
    generation: u16,
}

#[derive(Debug)]
pub struct TileIndex(u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileReference(u16, u16);

enum VRamState {
    ReferenceCounted(u16, TileReference),
    Free(u16),
}

impl VRamState {
    fn increase_reference(&mut self) {
        if let VRamState::ReferenceCounted(count, _) = self {
            *count += 1;
        } else {
            panic!("Corrupted vram state");
        }
    }

    fn decrease_reference(&mut self) -> (u16, TileReference) {
        if let VRamState::ReferenceCounted(count, tile_ref) = self {
            *count -= 1;
            (*count, *tile_ref)
        } else {
            panic!("Corrupted vram state");
        }
    }
}

enum ArenaStorageItem<T> {
    EndOfFreeList,
    NextFree(usize),
    Data(T, u16),
}

pub struct VRamManager<'a> {
    tilesets: Vec<ArenaStorageItem<TileSet<'a>>>,
    generation: u16,
    free_pointer: Option<usize>,

    tile_set_to_vram: HashMap<TileReference, (u16, u16)>,
    references: Vec<VRamState>,
    vram_free_pointer: Option<usize>,
}

const END_OF_FREE_LIST_MARKER: u16 = u16::MAX;

impl<'a> VRamManager<'a> {
    pub fn new() -> Self {
        Self {
            tilesets: Vec::new(),
            generation: 0,
            free_pointer: None,

            tile_set_to_vram: HashMap::new(),
            references: vec![VRamState::Free(0)],
            vram_free_pointer: None,
        }
    }

    pub fn add_tileset(&mut self, tileset: TileSet<'a>) -> TileSetReference {
        let generation = self.generation;
        self.generation = self.generation.wrapping_add(1);

        let tileset = ArenaStorageItem::Data(tileset, generation);

        let index = if let Some(ptr) = self.free_pointer.take() {
            match self.tilesets[ptr] {
                ArenaStorageItem::EndOfFreeList => {
                    self.tilesets[ptr] = tileset;
                    ptr
                }
                ArenaStorageItem::NextFree(next_free) => {
                    self.free_pointer = Some(next_free);
                    self.tilesets[ptr] = tileset;
                    ptr
                }
                _ => panic!("Free pointer shouldn't point to valid data"),
            }
        } else {
            self.tilesets.push(tileset);
            self.tilesets.len() - 1
        };

        TileSetReference::new(index as u16, generation)
    }

    pub fn remove_tileset(&mut self, tile_set_ref: TileSetReference) {
        let tileset = &self.tilesets[tile_set_ref.id as usize];

        match tileset {
            ArenaStorageItem::Data(_, generation) => {
                assert_eq!(
                    *generation, tile_set_ref.generation,
                    "Tileset generation must be the same when removing"
                );

                self.tilesets[tile_set_ref.id as usize] = if let Some(ptr) = self.free_pointer {
                    ArenaStorageItem::NextFree(ptr)
                } else {
                    ArenaStorageItem::EndOfFreeList
                };

                self.free_pointer = Some(tile_set_ref.id as usize);
            }
            _ => panic!("Already freed, probably a double free?"),
        }
    }

    fn add_tile(&mut self, tile_set_ref: TileSetReference, tile: u16) -> TileIndex {
        let tile_ref = TileReference(tile_set_ref.id, tile);
        if let Some(&reference) = self.tile_set_to_vram.get(&tile_ref) {
            if reference.1 == tile_set_ref.generation {
                self.references[reference.0 as usize].increase_reference();
                return TileIndex(reference.0 as u16);
            }
        }

        let index_to_copy_into = if let Some(ptr) = self.vram_free_pointer.take() {
            match self.references[ptr] {
                VRamState::Free(next_free) => {
                    if next_free != END_OF_FREE_LIST_MARKER {
                        self.vram_free_pointer = Some(next_free as usize);
                    }
                }
                VRamState::ReferenceCounted(_, _) => panic!("Corrupted tile reference state"),
            }

            self.references[ptr] = VRamState::ReferenceCounted(1, tile_ref);
            ptr
        } else {
            self.references
                .push(VRamState::ReferenceCounted(1, tile_ref));
            self.references.len() - 1
        };

        let tile_slice = if let ArenaStorageItem::Data(data, generation) =
            &self.tilesets[tile_set_ref.id as usize]
        {
            assert_eq!(
                *generation, tile_set_ref.generation,
                "Stale tile data requested"
            );

            let tile_offset = (tile as usize) * data.format.tile_size() / 4;
            &data.tiles[tile_offset..(tile_offset + data.format.tile_size() / 4)]
        } else {
            panic!("Cannot find tile data at given reference");
        };

        let tile_size_in_words = TileFormat::FourBpp.tile_size() / 4;

        for (i, &word) in tile_slice.iter().enumerate() {
            TILE_BACKGROUND.set(index_to_copy_into * tile_size_in_words + i, word);
        }

        self.tile_set_to_vram.insert(
            TileReference(tile_set_ref.id, tile),
            (index_to_copy_into as u16, tile_set_ref.generation),
        );

        TileIndex(index_to_copy_into as u16)
    }

    fn remove_tile(&mut self, tile_index: TileIndex) {
        let index = tile_index.0 as usize;

        let (new_count, tile_ref) = self.references[index].decrease_reference();

        if new_count != 0 {
            return;
        }

        if let Some(ptr) = self.vram_free_pointer {
            self.references[index] = VRamState::Free(ptr as u16);
        } else {
            self.references[index] = VRamState::Free(END_OF_FREE_LIST_MARKER);
        }

        self.tile_set_to_vram.remove(&tile_ref);

        self.vram_free_pointer = Some(index);
    }

    /// Copies raw palettes to the background palette without any checks.
    pub fn set_background_palette_raw(&mut self, palette: &[u16]) {
        for (index, &colour) in palette.iter().enumerate() {
            PALETTE_BACKGROUND.set(index, colour);
        }
    }

    fn set_background_palette(&mut self, pal_index: u8, palette: &palette16::Palette16) {
        for (colour_index, &colour) in palette.colours.iter().enumerate() {
            PALETTE_BACKGROUND.set(pal_index as usize * 16 + colour_index, colour);
        }
    }

    /// Copies palettes to the background palettes without any checks.
    pub fn set_background_palettes(&mut self, palettes: &[palette16::Palette16]) {
        for (palette_index, entry) in palettes.iter().enumerate() {
            self.set_background_palette(palette_index as u8, entry)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
struct Tile(u16);

impl Tile {
    fn new(idx: TileIndex, setting: TileSetting) -> Self {
        Self(idx.0 | setting.setting())
    }

    fn tile_index(self) -> TileIndex {
        TileIndex(self.0 & ((1 << 10) - 1))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TileSetting(u16);

impl TileSetting {
    pub const fn new(tile_id: u16, hflip: bool, vflip: bool, palette_id: u8) -> Self {
        Self(
            (tile_id & ((1 << 10) - 1))
                | ((hflip as u16) << 10)
                | ((vflip as u16) << 11)
                | ((palette_id as u16) << 12),
        )
    }

    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    fn index(self) -> u16 {
        self.0 & ((1 << 10) - 1)
    }

    fn setting(self) -> u16 {
        self.0 & !((1 << 10) - 1)
    }
}

pub struct RegularMap {
    background_id: u8,

    screenblock: u8,
    x_scroll: u16,
    y_scroll: u16,
    priority: Priority,

    tiles: [Tile; 32 * 32],
    tiles_dirty: bool,
}

impl RegularMap {
    fn new(background_id: u8, screenblock: u8, priority: Priority) -> Self {
        Self {
            background_id,

            screenblock,
            x_scroll: 0,
            y_scroll: 0,
            priority,

            tiles: [Tile::default(); 32 * 32],
            tiles_dirty: true,
        }
    }

    pub fn set_tile(
        &mut self,
        vram: &mut VRamManager,
        pos: Vector2D<u16>,
        tileset_ref: TileSetReference,
        tile_setting: TileSetting,
    ) {
        let pos = (pos.x + pos.y * 32) as usize;

        let old_tile = self.tiles[pos];
        if old_tile != Tile::default() {
            vram.remove_tile(old_tile.tile_index());
        }

        let tile_index = tile_setting.index();

        let new_tile = if tile_index != 0 {
            let new_tile_idx = vram.add_tile(tileset_ref, tile_index);
            Tile::new(new_tile_idx, tile_setting)
        } else {
            Tile::default()
        };

        if old_tile == new_tile {
            // no need to mark as dirty if nothing changes
            return;
        }

        self.tiles[pos] = new_tile;
        self.tiles_dirty = true;
    }

    pub fn clear(&mut self, vram: &mut VRamManager) {
        for tile in self.tiles.iter_mut() {
            vram.remove_tile(tile.tile_index());

            *tile = Tile::default();
        }
    }

    pub fn show(&mut self) {
        let mode = DISPLAY_CONTROL.get();
        let new_mode = mode | (1 << (self.background_id + 0x08));
        DISPLAY_CONTROL.set(new_mode);
    }

    pub fn hide(&mut self) {
        let mode = DISPLAY_CONTROL.get();
        let new_mode = mode & !(1 << (self.background_id + 0x08));
        DISPLAY_CONTROL.set(new_mode);
    }

    pub fn commit(&mut self) {
        let new_bg_control_value = (self.priority as u16) | ((self.screenblock as u16) << 8);

        self.bg_control_register().set(new_bg_control_value);
        self.bg_h_offset().set(self.x_scroll);
        self.bg_v_offset().set(self.y_scroll);

        if !self.tiles_dirty {
            return;
        }

        let screenblock_memory = self.screenblock_memory();

        let scroll_pos = self.get_scroll_pos();
        let start_x = scroll_pos.x / 8;
        let end_x = div_ceil(scroll_pos.x as i32 + display::WIDTH, 8) as u16 + 1;

        let start_y = scroll_pos.y / 8;
        let end_y = div_ceil(scroll_pos.y as i32 + display::HEIGHT, 8) as u16 + 1;

        for y in start_y..end_y {
            for x in start_x..end_x {
                let id = y.rem_euclid(32) * 32 + x.rem_euclid(32);
                screenblock_memory.set(id as usize, self.tiles[id as usize].0);
            }
        }

        self.tiles_dirty = false;
    }

    pub fn set_scroll_pos(&mut self, pos: Vector2D<u16>) {
        self.x_scroll = pos.x;
        self.y_scroll = pos.y;
    }

    pub fn get_scroll_pos(&self) -> Vector2D<u16> {
        (self.x_scroll, self.y_scroll).into()
    }

    const fn bg_control_register(&self) -> MemoryMapped<u16> {
        unsafe { MemoryMapped::new(0x0400_0008 + 2 * self.background_id as usize) }
    }

    const fn bg_h_offset(&self) -> MemoryMapped<u16> {
        unsafe { MemoryMapped::new(0x0400_0010 + 4 * self.background_id as usize) }
    }

    const fn bg_v_offset(&self) -> MemoryMapped<u16> {
        unsafe { MemoryMapped::new(0x0400_0012 + 4 * self.background_id as usize) }
    }

    const fn screenblock_memory(&self) -> MemoryMapped1DArray<u16, { 32 * 32 }> {
        unsafe { MemoryMapped1DArray::new(0x0600_0000 + 0x1000 * self.screenblock as usize / 2) }
    }
}

pub struct InfiniteScrolledMap<'a> {
    map: MapLoan<'a, RegularMap>,
    get_tile: Box<dyn Fn(Vector2D<i32>) -> (TileSetReference, TileSetting)>,

    current_pos: Vector2D<i32>,
    offset: Vector2D<i32>,
}

impl<'a> InfiniteScrolledMap<'a> {
    pub fn new(
        map: MapLoan<'a, RegularMap>,
        get_tile: Box<dyn Fn(Vector2D<i32>) -> (TileSetReference, TileSetting)>,
    ) -> Self {
        Self {
            map,
            get_tile,
            current_pos: (0, 0).into(),
            offset: (0, 0).into(),
        }
    }

    pub fn init(&mut self, vram: &mut VRamManager, pos: Vector2D<i32>) {
        self.current_pos = pos;

        let x_start = div_floor(self.current_pos.x, 8);
        let y_start = div_floor(self.current_pos.y, 8);

        let x_end = div_ceil(self.current_pos.x + display::WIDTH, 8) + 1;
        let y_end = div_ceil(self.current_pos.y + display::HEIGHT, 8) + 1;

        for (y_idx, y) in (y_start..y_end).enumerate() {
            for (x_idx, x) in (x_start..x_end).enumerate() {
                let pos = (x, y).into();
                let (tile_set_ref, tile_setting) = (self.get_tile)(pos);

                self.map.set_tile(
                    vram,
                    (x_idx as u16, y_idx as u16).into(),
                    tile_set_ref,
                    tile_setting,
                );
            }
        }

        let offset = self.current_pos - (x_start * 8, y_start * 8).into();
        let offset_scroll = (
            offset.x.rem_euclid(32 * 8) as u16,
            offset.y.rem_euclid(32 * 8) as u16,
        )
            .into();

        self.map.set_scroll_pos(offset_scroll);
        self.offset = (x_start, y_start).into();
    }

    pub fn set_pos(&mut self, vram: &mut VRamManager, new_pos: Vector2D<i32>) {
        let old_pos = self.current_pos;

        let difference = new_pos - old_pos;

        if difference.x.abs() > 10 * 8 || difference.y.abs() > 10 * 8 {
            self.init(vram, new_pos);
            return;
        }

        self.current_pos = new_pos;

        let new_tile_x = div_floor(new_pos.x, 8);
        let new_tile_y = div_floor(new_pos.y, 8);

        let difference_tile_x = div_ceil(difference.x, 8);
        let difference_tile_y = div_ceil(difference.y, 8);

        let vertical_rect_to_update: Rect<i32> = if div_floor(old_pos.x, 8) != new_tile_x {
            // need to update the x line
            // calculate which direction we need to update
            let direction = difference.x.signum();

            // either need to update 20 or 21 tiles depending on whether the y coordinate is a perfect multiple
            let y_tiles_to_update = 22;

            let line_to_update = if direction < 0 {
                // moving to the left, so need to update the left most position
                new_tile_x
            } else {
                // moving to the right, so need to update the right most position
                new_tile_x + 30 // TODO is this correct?
            };

            Rect::new(
                (line_to_update, new_tile_y - 1).into(),
                (difference_tile_x, y_tiles_to_update).into(),
            )
        } else {
            Rect::new((0i32, 0).into(), (0i32, 0).into())
        };

        let horizontal_rect_to_update: Rect<i32> = if div_floor(old_pos.y, 8) != new_tile_y {
            // need to update the y line
            // calculate which direction we need to update
            let direction = difference.y.signum();

            // either need to update 30 or 31 tiles depending on whether the x coordinate is a perfect multiple
            let x_tiles_to_update: i32 = 32;

            let line_to_update = if direction < 0 {
                // moving up so need to update the top
                new_tile_y
            } else {
                // moving down so need to update the bottom
                new_tile_y + 20 // TODO is this correct?
            };

            Rect::new(
                (new_tile_x - 1, line_to_update).into(),
                (x_tiles_to_update, difference_tile_y).into(),
            )
        } else {
            Rect::new((0i32, 0).into(), (0i32, 0).into())
        };

        for (tile_x, tile_y) in vertical_rect_to_update
            .iter()
            .chain(horizontal_rect_to_update.iter())
        {
            let (tile_set_ref, tile_setting) = (self.get_tile)((tile_x, tile_y).into());

            self.map.set_tile(
                vram,
                (
                    (tile_x - self.offset.x).rem_euclid(32) as u16,
                    (tile_y - self.offset.y).rem_euclid(32) as u16,
                )
                    .into(),
                tile_set_ref,
                tile_setting,
            );
        }

        let current_scroll = self.map.get_scroll_pos();
        let new_scroll = (
            (current_scroll.x as i32 + difference.x).rem_euclid(32 * 8) as u16,
            (current_scroll.y as i32 + difference.y).rem_euclid(32 * 8) as u16,
        )
            .into();

        self.map.set_scroll_pos(new_scroll);
    }

    pub fn show(&mut self) {
        self.map.show();
    }

    pub fn hide(&mut self) {
        self.map.hide();
    }

    pub fn commit(&mut self) {
        self.map.commit();
    }
}

fn div_floor(x: i32, y: i32) -> i32 {
    if x > 0 && y < 0 {
        (x - 1) / y - 1
    } else if x < 0 && y > 0 {
        (x + 1) / y - 1
    } else {
        x / y
    }
}

fn div_ceil(x: i32, y: i32) -> i32 {
    if x > 0 && y > 0 {
        (x - 1) / y + 1
    } else if x < 0 && y < 0 {
        (x + 1) / y + 1
    } else {
        x / y
    }
}

pub struct Tiled0 {
    regular: RefCell<Bitarray<1>>,
}

impl Tiled0 {
    pub(crate) unsafe fn new() -> Self {
        set_graphics_settings(GraphicsSettings::empty() | GraphicsSettings::SPRITE1_D);
        set_graphics_mode(DisplayMode::Tiled0);

        Self {
            regular: Default::default(),
        }
    }

    pub fn background(&self, priority: Priority) -> MapLoan<'_, RegularMap> {
        let mut regular = self.regular.borrow_mut();
        let new_background = regular.first_zero().unwrap();
        if new_background >= 4 {
            panic!("can only have 4 active backgrounds");
        }

        let bg = RegularMap::new(new_background as u8, (new_background + 16) as u8, priority);

        regular.set(new_background, true);

        MapLoan::new(bg, new_background as u8, &self.regular)
    }
}

impl TileSetReference {
    fn new(id: u16, generation: u16) -> Self {
        Self { id, generation }
    }
}

pub struct MapLoan<'a, T> {
    map: T,
    background_id: u8,
    regular_map_list: &'a RefCell<Bitarray<1>>,
}

impl<'a, T> Deref for MapLoan<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl<'a, T> DerefMut for MapLoan<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.map
    }
}

impl<'a, T> MapLoan<'a, T> {
    fn new(map: T, background_id: u8, regular_map_list: &'a RefCell<Bitarray<1>>) -> Self {
        MapLoan {
            map,
            background_id,
            regular_map_list,
        }
    }
}

impl<'a, T> Drop for MapLoan<'a, T> {
    fn drop(&mut self) {
        self.regular_map_list
            .borrow_mut()
            .set(self.background_id as usize, false);
    }
}
