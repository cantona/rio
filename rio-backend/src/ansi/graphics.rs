// graphics.rs was retired from a alacritty PR made by ayosec
// Alacritty is licensed under Apache 2.0 license.
// https://github.com/alacritty/alacritty/pull/4763/files

use crate::ansi::sixel;
use crate::config::colors::ColorRgb;
use crate::crosswords::grid::Dimensions;
use crate::sugarloaf::{GraphicData, GraphicId};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::mem;
use std::sync::{Arc, Weak};
use tracing::debug;

/// A graphic scheduled for removal, tagged with the id space it lives in.
///
/// Atlas graphics (sixel/iTerm2) and kitty images don't share an id
/// space: both allocate per terminal and can collide numerically. The
/// window-level store is keyed by `sugarloaf::ImageKey` (route, source,
/// id), and the frontend needs this tag to build the source part — a
/// bare id can't tell the removal handler which entry to target, so a
/// kitty removal could delete an atlas entry and leak the real one.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GraphicRemoval {
    /// Sixel/iTerm2 atlas graphic (`GraphicId` space).
    Atlas(GraphicId),
    /// Kitty image (raw protocol `image_id` space).
    Kitty(u32),
}

#[derive(Debug, Clone)]
pub struct UpdateQueues {
    /// Atlas graphics (sixel/iTerm2) read from the PTY.
    pub pending: Vec<GraphicData>,

    /// Image textures (kitty) keyed by image_id.
    pub pending_images: Vec<(u32, GraphicData)>,

    /// Graphics removed from the grid, tagged with their id space.
    pub remove_queue: Vec<GraphicRemoval>,
}

#[derive(Clone, Debug)]
pub struct TextureRef {
    /// Graphic identifier.
    pub id: GraphicId,

    /// Width, in pixels, of the graphic.
    pub width: u16,

    /// Height, in pixels, of the graphic.
    pub height: u16,

    /// Width, in pixels, of the cell when the graphic was inserted.
    pub cell_width: usize,

    /// Height, in pixels, of the cell when the graphic was inserted.
    pub cell_height: usize,

    /// Queue to track removed textures.
    pub texture_operations: Weak<Mutex<Vec<GraphicRemoval>>>,
}

impl PartialEq for TextureRef {
    fn eq(&self, t: &Self) -> bool {
        // Ignore texture_operations.
        self.id == t.id
    }
}

impl Eq for TextureRef {}

impl Drop for TextureRef {
    fn drop(&mut self) {
        if let Some(texture_operations) = self.texture_operations.upgrade() {
            texture_operations
                .lock()
                .push(GraphicRemoval::Atlas(self.id));
        }
    }
}

/// A list of graphics in a single cell.
pub type GraphicsCell = SmallVec<[GraphicCell; 1]>;

/// Graphic data stored in a cell's extras slot.
///
/// One `GraphicCell` (one extras slot) is shared by every covered cell of
/// an image row — a slot per cell would exhaust the u16 extras id space in
/// a dozen large images. A cell's actual x offset is derived positionally:
/// `offset_x + (col - anchor_col) * texture.cell_width`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphicCell {
    /// Texture to draw the graphic in this cell.
    pub texture: Arc<TextureRef>,

    /// Offset in the x direction, at `anchor_col`.
    pub offset_x: u16,

    /// Offset in the y direction.
    pub offset_y: u16,

    /// Grid column `offset_x` is measured at.
    pub anchor_col: u16,
}

/// Kitty graphics Unicode placeholder character
pub const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

/// Stored image data for Kitty graphics protocol
#[derive(Debug, Clone, PartialEq)]
pub struct StoredImage {
    pub data: GraphicData,
    pub transmission_time: std::time::Instant,
}

/// Overlay placement for a kitty graphics image.
/// Stored separately from grid cells — rendered as an overlay layer.
///
/// Kitty images use the protocol's `image_id: u32` directly, not `GraphicId`.
/// `GraphicId` is for atlas-based graphics (sixel/iTerm2) which share a
/// sequential ID space. Kitty image_ids come from the protocol and would
/// collide with atlas IDs. They also use a completely separate rendering
/// pipeline (per-image GPU textures, not atlas), so there's no reason to
/// wrap them in `GraphicId`.
#[derive(Debug, Clone, PartialEq)]
pub struct KittyPlacement {
    /// Kitty protocol image ID (i= parameter).
    pub image_id: u32,
    /// Kitty protocol placement ID (p= parameter).
    pub placement_id: u32,
    /// Source rectangle within the image (pixels).
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    /// Grid column of the top-left corner.
    pub dest_col: usize,
    /// Absolute row (scrollback-aware) of the top-left corner.
    pub dest_row: i64,
    /// Display size in cells.
    pub columns: u32,
    pub rows: u32,
    /// Actual display pixel dimensions.
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Sub-cell pixel offset.
    pub cell_x_offset: u32,
    pub cell_y_offset: u32,
    /// Z-index layer for rendering order.
    pub z_index: i32,
    /// Transmission timestamp for cache invalidation.
    pub transmit_time: std::time::Instant,
}

/// Virtual placement metadata for Kitty graphics protocol
/// Stored separately from direct graphics in cells
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualPlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub columns: u32,
    pub rows: u32,
    pub x: u32,
    pub y: u32,
}

/// Per-screen Kitty graphics state.
///
/// Each terminal has two screens (main and alt). Per the kitty graphics
/// spec each screen owns its own image cache, placements, and number
/// mappings, so swapping into the alt screen hides main-screen images
/// (and vice versa) instead of leaking them across the boundary.
///
/// `Graphics` keeps the *active* screen's state inline (so existing
/// rendering code can read `graphics.kitty_*` directly without changes)
/// and stores the *inactive* screen's state in this struct. On
/// `swap_kitty_screen_state` the two are exchanged with `mem::swap`.
#[derive(Debug, Default)]
pub struct KittyScreenState {
    pub kitty_images: FxHashMap<u32, StoredImage>,
    pub kitty_image_numbers: FxHashMap<u32, u32>,
    pub kitty_placements: FxHashMap<(u32, u32), KittyPlacement>,
    pub kitty_virtual_placements: FxHashMap<(u32, u32), VirtualPlacement>,
}

/// Track changes in the grid to add or to remove graphics.
#[derive(Debug)]
pub struct Graphics {
    /// Last generated identifier.
    pub last_id: u64,

    /// New atlas graphics (sixel/iTerm2), received from the PTY.
    pub pending: Vec<GraphicData>,

    /// New image textures (kitty), keyed by image_id.
    pub pending_images: Vec<(u32, GraphicData)>,

    /// Graphics removed from the grid, tagged with their id space.
    pub texture_operations: Arc<Mutex<Vec<GraphicRemoval>>>,

    /// Shared palette for Sixel graphics.
    pub sixel_shared_palette: Option<Vec<ColorRgb>>,

    /// Cell height in pixels.
    pub cell_height: f32,

    /// Cell width in pixels.
    pub cell_width: f32,

    /// Current Sixel parser.
    pub sixel_parser: Option<Box<sixel::Parser>>,

    /// Kitty graphics: Cache of transmitted images (by image_id)
    /// Allows placing the same image multiple times without re-transmission
    pub kitty_images: FxHashMap<u32, StoredImage>,

    /// Kitty graphics: Image number to ID mapping (for I= parameter)
    /// Maps image number to the most recently transmitted image with that number
    pub kitty_image_numbers: FxHashMap<u32, u32>,

    /// Kitty graphics: Virtual placements (when U=1)
    /// Key is (image_id, placement_id), value is placement metadata
    pub kitty_virtual_placements: FxHashMap<(u32, u32), VirtualPlacement>,

    /// Kitty graphics: State for chunked image transmissions
    /// Stores incomplete transmissions and tracks current transmission key
    pub kitty_chunking_state: crate::ansi::kitty_graphics_protocol::KittyGraphicsState,

    /// Total bytes of image data currently stored in memory
    /// Includes both pending graphics and stored Kitty images
    pub total_bytes: usize,

    /// Memory limit for graphics storage (default 320MB per kitty spec)
    /// If this is exceeded, oldest/unused images will be evicted
    pub total_limit: usize,

    /// Tracks when each graphic was added (for eviction priority)
    /// Maps GraphicId to insertion timestamp
    pub image_timestamps: FxHashMap<GraphicId, std::time::Instant>,

    /// Byte size of each tracked atlas graphic, so `untrack_graphic` can
    /// subtract the exact amount from `total_bytes` when a graphic's last
    /// referencing cell is dropped (the removal path only knows the id).
    pub graphic_bytes: FxHashMap<GraphicId, usize>,

    /// Weak references to placed textures, for O(1) liveness checks.
    /// Avoids scanning the entire grid to find which graphics are in use.
    /// When the Arc<TextureRef> in grid cells is fully dropped, the Weak
    /// will report strong_count() == 0, meaning the graphic is no longer displayed.
    pub placed_textures: FxHashMap<GraphicId, Weak<TextureRef>>,

    /// Kitty graphics: Overlay placements.
    /// Key is (image_id, placement_id). Rendered as overlays, not in grid cells.
    pub kitty_placements: FxHashMap<(u32, u32), KittyPlacement>,

    /// Kitty graphics state for the *inactive* screen.
    /// When the terminal toggles between main and alt screens this is
    /// swapped with the active fields (`kitty_images`, `kitty_placements`,
    /// `kitty_image_numbers`, `kitty_virtual_placements`) so each screen
    /// keeps its own image set.
    pub kitty_inactive_screen: KittyScreenState,

    /// Counter for auto-assigning internal placement IDs.
    ///
    /// Per kitty spec, when a client asks to place an image without an
    /// explicit `p=` (or with `p=0`), the terminal must allocate a
    /// unique placement_id internally so multiple placements of the
    /// same image don't collide at key `(image_id, 0)`. We allocate
    /// from `0x80000000..` so internal IDs do not collide with the
    /// client-supplied range (`1..0x80000000`).
    pub next_internal_placement_id: u32,

    /// Signals the renderer that overlay placements have changed.
    pub kitty_graphics_dirty: bool,

    /// Set when an over-budget insert could not free enough space
    /// (everything left is live). Gates the per-insert gc/evict scan
    /// and warning until accounting drops below `total_limit` again.
    pub over_budget_warned: bool,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            last_id: 0,
            pending: Vec::new(),
            pending_images: Vec::new(),
            texture_operations: Arc::new(Mutex::new(Vec::new())),
            sixel_shared_palette: None,
            cell_height: 0.0,
            cell_width: 0.0,
            sixel_parser: None,
            kitty_images: FxHashMap::default(),
            kitty_image_numbers: FxHashMap::default(),
            kitty_virtual_placements: FxHashMap::default(),
            kitty_chunking_state:
                crate::ansi::kitty_graphics_protocol::KittyGraphicsState::default(),
            total_bytes: 0,
            total_limit: 320 * 1024 * 1024, // 320MB per kitty spec
            image_timestamps: FxHashMap::default(),
            graphic_bytes: FxHashMap::default(),
            placed_textures: FxHashMap::default(),
            kitty_placements: FxHashMap::default(),
            kitty_inactive_screen: KittyScreenState::default(),
            next_internal_placement_id: 0,
            kitty_graphics_dirty: false,
            over_budget_warned: false,
        }
    }
}

impl Graphics {
    /// Create a new instance, and initialize it with the dimensions of the
    /// window.
    pub fn new<S: Dimensions>(size: &S) -> Self {
        let mut graphics = Graphics::default();
        graphics.resize(size);
        graphics
    }

    /// Generate a new graphic identifier (for sixel/iTerm2 atlas graphics).
    pub fn next_id(&mut self) -> GraphicId {
        self.last_id += 1;
        GraphicId::new(self.last_id)
    }

    /// Get queues to update graphics in the grid.
    ///
    /// If all queues are empty, it returns `None`.
    pub fn has_pending_updates(&self) -> bool {
        !self.pending.is_empty()
            || !self.pending_images.is_empty()
            || !self.texture_operations.lock().is_empty()
    }

    pub fn take_queues(&mut self) -> Option<UpdateQueues> {
        let remove_queue = {
            let mut queue = self.texture_operations.lock();
            if queue.is_empty() {
                Vec::new()
            } else {
                mem::take(&mut *queue)
            }
        };

        if remove_queue.is_empty()
            && self.pending.is_empty()
            && self.pending_images.is_empty()
        {
            return None;
        }

        // Deflate the byte accounting for atlas graphics whose last
        // referencing cell was just dropped. Kitty removals deflate at
        // their delete/evict site, where the image size is still known.
        for removal in &remove_queue {
            if let GraphicRemoval::Atlas(id) = removal {
                self.untrack_graphic_by_id(*id);
            }
        }

        Some(UpdateQueues {
            pending: mem::take(&mut self.pending),
            pending_images: mem::take(&mut self.pending_images),
            remove_queue,
        })
    }

    /// Update cell dimensions.
    pub fn resize<S: Dimensions>(&mut self, size: &S) {
        self.cell_height = size.square_height();
        self.cell_width = size.square_width();
    }

    /// Allocate a unique internal placement_id.
    ///
    /// Used by `place_kitty_overlay` whenever a placement request comes
    /// in with `placement_id == 0`. Without this, two `a=T` calls (or a
    /// client running `kitten icat` repeatedly) for the same image_id
    /// would each insert at key `(image_id, 0)` and the second would
    /// silently overwrite the first.
    pub fn allocate_internal_placement_id(&mut self) -> u32 {
        if self.next_internal_placement_id < 0x80000000 {
            self.next_internal_placement_id = 0x80000000;
        }
        let id = self.next_internal_placement_id;
        self.next_internal_placement_id = self
            .next_internal_placement_id
            .checked_add(1)
            .unwrap_or(0x80000000);
        id
    }

    /// Swap kitty graphics state between the active and inactive screen.
    ///
    /// Called by `Crosswords::swap_alt` so each screen (main vs alt)
    /// keeps its own image cache, placements, number mappings, and
    /// virtual placements. Marks the kitty overlay layer dirty so the
    /// renderer rebuilds its overlay set against the new active screen.
    pub fn swap_kitty_screen_state(&mut self) {
        std::mem::swap(
            &mut self.kitty_images,
            &mut self.kitty_inactive_screen.kitty_images,
        );
        std::mem::swap(
            &mut self.kitty_image_numbers,
            &mut self.kitty_inactive_screen.kitty_image_numbers,
        );
        std::mem::swap(
            &mut self.kitty_placements,
            &mut self.kitty_inactive_screen.kitty_placements,
        );
        std::mem::swap(
            &mut self.kitty_virtual_placements,
            &mut self.kitty_inactive_screen.kitty_virtual_placements,
        );
        self.kitty_graphics_dirty = true;
    }

    /// Clear all kitty graphics state on both screens. Used by full reset.
    pub fn clear_all_kitty_state(&mut self) {
        // total_bytes is the *global* counter and every stored image on
        // either screen inflated it once, so deflate per entry. Both
        // screens drop their copy, so the window-level texture goes too —
        // queue each id once (the two screens may share an id).
        let mut freed = 0usize;
        {
            let mut removals = self.texture_operations.lock();
            for (&id, stored) in &self.kitty_images {
                freed += stored.data.pixels.len();
                removals.push(GraphicRemoval::Kitty(id));
            }
            for (&id, stored) in &self.kitty_inactive_screen.kitty_images {
                freed += stored.data.pixels.len();
                if !self.kitty_images.contains_key(&id) {
                    removals.push(GraphicRemoval::Kitty(id));
                }
            }
        }
        self.deflate_total_bytes(freed);

        self.kitty_images.clear();
        self.kitty_image_numbers.clear();
        self.kitty_placements.clear();
        self.kitty_virtual_placements.clear();
        self.kitty_inactive_screen = KittyScreenState::default();
        self.kitty_graphics_dirty = true;
    }

    /// Store a kitty graphics image for later placement.
    /// Evicts old images if over memory limit.
    pub fn store_kitty_image(
        &mut self,
        image_id: u32,
        image_number: Option<u32>,
        mut data: GraphicData,
    ) {
        let now = std::time::Instant::now();
        data.transmit_time = now;

        // Evict before storing to protect images with active placements
        let new_bytes = data.pixels.len();
        if self.total_bytes + new_bytes > self.total_limit {
            // Collect active IDs — images with placements are protected
            let mut active = std::collections::HashSet::new();
            for placement in self.kitty_placements.values() {
                active.insert(placement.image_id as u64);
            }
            // Also protect the image we're about to store
            active.insert(image_id as u64);
            self.evict_images(new_bytes, &active);
        }

        // If replacing an existing image, subtract its bytes first
        if let Some(old) = self.kitty_images.get(&image_id) {
            let old_bytes = old.data.pixels.len();
            self.deflate_total_bytes(old_bytes);
        }

        self.kitty_images.insert(
            image_id,
            StoredImage {
                data,
                transmission_time: now,
            },
        );
        self.total_bytes += new_bytes;
        self.kitty_graphics_dirty = true;

        // Update image number mapping if provided
        if let Some(number) = image_number {
            self.kitty_image_numbers.insert(number, image_id);
        }
    }

    /// Get a stored kitty graphics image by ID
    pub fn get_kitty_image(&self, image_id: u32) -> Option<&StoredImage> {
        self.kitty_images.get(&image_id)
    }

    /// Get a stored kitty graphics image by number (I= parameter)
    /// Returns the most recently transmitted image with that number
    pub fn get_kitty_image_by_number(&self, image_number: u32) -> Option<&StoredImage> {
        self.kitty_image_numbers
            .get(&image_number)
            .and_then(|id| self.kitty_images.get(id))
    }

    /// Delete kitty graphics images from the active screen's cache.
    ///
    /// Deflates `total_bytes` for every image actually dropped and queues
    /// a `GraphicRemoval::Kitty` — unless the inactive screen still holds
    /// the same id: both screens share the window-level texture key, so
    /// the texture must survive until neither screen references it.
    pub fn delete_kitty_images(
        &mut self,
        predicate: impl Fn(&u32, &StoredImage) -> bool,
    ) {
        let before = self.kitty_images.len();
        let inactive = &self.kitty_inactive_screen.kitty_images;
        let mut freed = 0usize;
        let mut removals: Vec<GraphicRemoval> = Vec::new();
        self.kitty_images.retain(|id, img| {
            if !predicate(id, img) {
                return true;
            }
            freed += img.data.pixels.len();
            if !inactive.contains_key(id) {
                removals.push(GraphicRemoval::Kitty(*id));
            }
            false
        });
        if self.kitty_images.len() == before {
            return;
        }
        self.kitty_graphics_dirty = true;
        self.deflate_total_bytes(freed);
        if !removals.is_empty() {
            self.texture_operations.lock().append(&mut removals);
        }
        // Clean up stale number mappings
        self.kitty_image_numbers
            .retain(|_, id| self.kitty_images.contains_key(id));
    }

    /// Calculate the memory size of a graphic in bytes
    fn calculate_graphic_bytes(graphic: &GraphicData) -> usize {
        graphic.pixels.len()
    }

    /// Evict images to make space for required_bytes.
    /// Returns true if enough space was freed, false otherwise.
    ///
    /// Eviction priority (per kitty spec, extended for per-screen state):
    /// 1. Inactive-screen images (the user is not looking at them)
    /// 2. Active-screen unused images (no live placement)
    /// 3. Active-screen used images (visible — last resort)
    ///
    /// Within each tier we evict oldest by timestamp first.
    pub fn evict_images(
        &mut self,
        required_bytes: usize,
        used_ids: &std::collections::HashSet<u64>,
    ) -> bool {
        use tracing::debug;

        if self.total_bytes + required_bytes <= self.total_limit {
            return true; // No eviction needed
        }

        let bytes_to_free = (self.total_bytes + required_bytes) - self.total_limit;
        debug!("Graphics memory: need to evict {} bytes (current: {}, limit: {}, required: {})",
            bytes_to_free, self.total_bytes, self.total_limit, required_bytes);

        /// Where an eviction candidate lives, so removal touches the
        /// right map. `Pending` is a sixel/iTerm2 atlas graphic in
        /// `self.pending`; the kitty variants are screen-scoped.
        #[derive(Copy, Clone, PartialEq, Eq)]
        enum CandidateSource {
            Pending,
            ActiveKitty,
            InactiveKitty,
        }

        // Tier scale: lower number = evict first
        // 0 = inactive kitty, 1 = active unused, 2 = active used (pending or kitty)
        fn tier_for(source: CandidateSource, is_used: bool) -> u8 {
            match source {
                CandidateSource::InactiveKitty => 0,
                CandidateSource::Pending | CandidateSource::ActiveKitty => {
                    if is_used {
                        2
                    } else {
                        1
                    }
                }
            }
        }

        // Candidate: (sentinel GraphicId, timestamp, is_used, bytes, source)
        let mut candidates: Vec<(
            GraphicId,
            std::time::Instant,
            bool,
            usize,
            CandidateSource,
        )> = Vec::new();

        // Check pending graphics (sixel/iTerm2 — atlas based, single screen)
        for graphic in &self.pending {
            if let Some(&timestamp) = self.image_timestamps.get(&graphic.id) {
                let is_used = used_ids.contains(&graphic.id.get());
                let bytes = Self::calculate_graphic_bytes(graphic);
                candidates.push((
                    graphic.id,
                    timestamp,
                    is_used,
                    bytes,
                    CandidateSource::Pending,
                ));
            }
        }

        // Check active-screen kitty images
        for (&kitty_id, stored) in &self.kitty_images {
            let id_as_u64 = kitty_id as u64;
            let is_used = used_ids.contains(&id_as_u64);
            let bytes = Self::calculate_graphic_bytes(&stored.data);
            candidates.push((
                GraphicId::new(id_as_u64),
                stored.transmission_time,
                is_used,
                bytes,
                CandidateSource::ActiveKitty,
            ));
        }

        // Check inactive-screen kitty images. These are not visible to
        // the user, so they're the first tier to evict regardless of
        // whether they have a placement.
        for (&kitty_id, stored) in &self.kitty_inactive_screen.kitty_images {
            let id_as_u64 = kitty_id as u64;
            let bytes = Self::calculate_graphic_bytes(&stored.data);
            candidates.push((
                GraphicId::new(id_as_u64),
                stored.transmission_time,
                false, // not displayed (we're on the other screen)
                bytes,
                CandidateSource::InactiveKitty,
            ));
        }

        if candidates.is_empty() {
            debug!("No candidates for eviction");
            return false;
        }

        // Sort by tier (ascending), then oldest first within tier.
        candidates.sort_by(|a, b| {
            let ta = tier_for(a.4, a.2);
            let tb = tier_for(b.4, b.2);
            ta.cmp(&tb).then_with(|| a.1.cmp(&b.1))
        });

        let mut freed_bytes = 0usize;
        let mut evicted: Vec<(GraphicId, CandidateSource)> = Vec::new();

        for (graphic_id, _, is_used, bytes, source) in candidates {
            if freed_bytes >= bytes_to_free {
                break;
            }

            evicted.push((graphic_id, source));
            freed_bytes += bytes;

            debug!(
                "Evicting graphic id={}, bytes={}, used={}",
                graphic_id.get(),
                bytes,
                is_used
            );
        }

        // Actually remove the evicted graphics from the right home.
        for (id, source) in evicted {
            let evicted_u32 = id.get() as u32;
            // Tag the removal with its id space so the handler targets the
            // correct `image_data` key — an atlas key for `Pending`, the
            // raw protocol id for either kitty screen. A kitty id living
            // on both screens shares one window-level texture: queue its
            // removal only once neither screen references it.
            let removal = match source {
                CandidateSource::Pending => {
                    self.pending.retain(|g| g.id != id);
                    // Byte size is already accounted below via freed_bytes,
                    // but the per-id bookkeeping still needs clearing.
                    self.graphic_bytes.remove(&id);
                    Some(GraphicRemoval::Atlas(id))
                }
                CandidateSource::ActiveKitty => {
                    self.kitty_images.remove(&evicted_u32);
                    self.kitty_image_numbers.retain(|_, v| *v != evicted_u32);
                    self.kitty_graphics_dirty = true;
                    (!self
                        .kitty_inactive_screen
                        .kitty_images
                        .contains_key(&evicted_u32))
                    .then_some(GraphicRemoval::Kitty(evicted_u32))
                }
                CandidateSource::InactiveKitty => {
                    self.kitty_inactive_screen.kitty_images.remove(&evicted_u32);
                    self.kitty_inactive_screen
                        .kitty_image_numbers
                        .retain(|_, v| *v != evicted_u32);
                    (!self.kitty_images.contains_key(&evicted_u32))
                        .then_some(GraphicRemoval::Kitty(evicted_u32))
                }
            };

            // Remove timestamp (only used for pending atlas graphics)
            self.image_timestamps.remove(&id);

            // Add to removal queue so GPU textures get cleaned up
            if let Some(removal) = removal {
                self.texture_operations.lock().push(removal);
            }
        }

        // Sweep dangling placements on both screens. A placement is
        // dangling if its referenced image_id is no longer in the
        // matching screen's image cache. This catches both:
        //   - kitty placements whose image was just evicted
        //   - cross-namespace coincidences where a sixel/iTerm2 atlas
        //     graphic with the same numeric id as a kitty image is
        //     evicted (the test_eviction_removes_dangling_placements
        //     test pins this defensive behaviour)
        let active_ids: std::collections::HashSet<u32> =
            self.kitty_images.keys().copied().collect();
        self.kitty_placements
            .retain(|_, p| active_ids.contains(&p.image_id));
        let inactive_ids: std::collections::HashSet<u32> = self
            .kitty_inactive_screen
            .kitty_images
            .keys()
            .copied()
            .collect();
        self.kitty_inactive_screen
            .kitty_placements
            .retain(|_, p| inactive_ids.contains(&p.image_id));

        // Update total_bytes
        self.deflate_total_bytes(freed_bytes);

        debug!(
            "Evicted {} bytes, new total: {}",
            freed_bytes, self.total_bytes
        );
        freed_bytes >= bytes_to_free
    }

    /// Register a placed texture for liveness tracking.
    /// Call this after creating the Arc<TextureRef> in insert_graphic.
    pub fn register_placed_texture(
        &mut self,
        graphic_id: GraphicId,
        weak: Weak<TextureRef>,
    ) {
        self.placed_textures.insert(graphic_id, weak);
    }

    /// Collect IDs of graphics still displayed in the grid or as overlays.
    /// O(number of placements) instead of O(rows * cols).
    pub fn collect_active_graphic_ids(&mut self) -> std::collections::HashSet<u64> {
        // Clean up stale entries and collect live ones in one pass
        let mut active = std::collections::HashSet::new();
        // Cell-based (sixel) liveness
        self.placed_textures.retain(|id, weak| {
            if weak.strong_count() > 0 {
                active.insert(id.get());
                true
            } else {
                false
            }
        });
        // Overlay-based (kitty) liveness — use image_id directly
        for placement in self.kitty_placements.values() {
            active.insert(placement.image_id as u64);
        }
        active
    }

    /// Track a new graphic's memory usage and timestamp
    pub fn track_graphic(&mut self, graphic_id: GraphicId, bytes: usize) {
        self.image_timestamps
            .insert(graphic_id, std::time::Instant::now());
        self.graphic_bytes.insert(graphic_id, bytes);
        self.total_bytes += bytes;
        debug!(
            "Tracked graphic id={}, bytes={}, total_bytes={}",
            graphic_id.0, bytes, self.total_bytes
        );
    }

    /// Subtract freed bytes, releasing the over-budget warn latch as
    /// soon as accounting drops back under the limit so the next insert
    /// resumes normal gc/evict behavior.
    fn deflate_total_bytes(&mut self, bytes: usize) {
        self.total_bytes = self.total_bytes.saturating_sub(bytes);
        if self.total_bytes < self.total_limit {
            self.over_budget_warned = false;
        }
    }

    /// Update total_bytes when a graphic is removed
    pub fn untrack_graphic(&mut self, graphic_id: GraphicId, bytes: usize) {
        self.image_timestamps.remove(&graphic_id);
        self.graphic_bytes.remove(&graphic_id);
        self.deflate_total_bytes(bytes);
        debug!(
            "Untracked graphic id={}, bytes={}, total_bytes={}",
            graphic_id.0, bytes, self.total_bytes
        );
    }

    /// Untrack an atlas graphic whose last referencing cell was just
    /// dropped (id known, byte size looked up from `graphic_bytes`).
    ///
    /// Called from the removal queue drain so `total_bytes` deflates as
    /// soon as a graphic is freed rather than ratcheting forever — the
    /// 320MB accounting then reflects live pixel data. No-op if the id
    /// was already untracked (idempotent under duplicate queue entries).
    pub fn untrack_graphic_by_id(&mut self, graphic_id: GraphicId) {
        if let Some(bytes) = self.graphic_bytes.remove(&graphic_id) {
            self.image_timestamps.remove(&graphic_id);
            self.deflate_total_bytes(bytes);
            debug!(
                "Untracked graphic id={}, bytes={}, total_bytes={}",
                graphic_id.0, bytes, self.total_bytes
            );
        }
    }

    /// Deflate accounting for atlas removals still parked in the renderer
    /// queue, without draining it. `take_queues` does the same on drain;
    /// doing it eagerly lets the insert path see gc-freed bytes before
    /// deciding whether to evict live images. Idempotent —
    /// `untrack_graphic_by_id` no-ops on already-untracked ids.
    pub fn untrack_queued_removals(&mut self) {
        let ops = Arc::clone(&self.texture_operations);
        let queue = ops.lock();
        for removal in queue.iter() {
            if let GraphicRemoval::Atlas(id) = removal {
                self.untrack_graphic_by_id(*id);
            }
        }
    }
}

#[test]
fn check_opaque_region() {
    use sugarloaf::ColorType;
    let graphic = GraphicData {
        id: GraphicId::new(1),
        width: 10,
        height: 10,
        color_type: ColorType::Rgb,
        pixels: vec![255; 10 * 10 * 3],
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };

    assert!(graphic.is_filled(1, 1, 3, 3));
    assert!(!graphic.is_filled(8, 8, 10, 10));

    let pixels = {
        // Put a transparent 3x3 box inside the picture.
        let mut data = vec![255; 10 * 10 * 4];
        for y in 3..6 {
            let offset = y * 10 * 4;
            data[offset..offset + 3 * 4].fill(0);
        }
        data
    };

    let graphic = GraphicData {
        id: GraphicId::new(1),
        pixels,
        width: 10,
        height: 10,
        color_type: ColorType::Rgba,
        is_opaque: false,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };

    assert!(graphic.is_filled(0, 0, 3, 3));
    assert!(!graphic.is_filled(1, 1, 4, 4));
}

#[cfg(test)]
fn test_kitty_pixels(bytes: usize) -> GraphicData {
    GraphicData {
        id: GraphicId::new(0),
        width: 1,
        height: 1,
        color_type: sugarloaf::ColorType::Rgba,
        pixels: vec![255u8; bytes],
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    }
}

#[test]
fn test_delete_kitty_images_deflates_and_queues_removal() {
    let mut graphics = Graphics::default();
    graphics.store_kitty_image(1, Some(9), test_kitty_pixels(64));
    graphics.store_kitty_image(2, None, test_kitty_pixels(32));
    assert_eq!(graphics.total_bytes, 96);

    graphics.delete_kitty_images(|id, _| *id == 1);

    assert_eq!(graphics.total_bytes, 32);
    assert!(graphics.kitty_image_numbers.is_empty());
    assert_eq!(
        graphics.texture_operations.lock().as_slice(),
        &[GraphicRemoval::Kitty(1)]
    );
}

#[test]
fn test_delete_kitty_image_shared_with_inactive_screen() {
    let mut graphics = Graphics::default();
    graphics.store_kitty_image(1, None, test_kitty_pixels(64));
    graphics.swap_kitty_screen_state();
    graphics.store_kitty_image(1, None, test_kitty_pixels(32));
    assert_eq!(graphics.total_bytes, 96);

    graphics.delete_kitty_images(|_, _| true);

    // Only the active copy's bytes deflate; the inactive screen still
    // owns the window-level texture, so no removal may be queued.
    assert_eq!(graphics.total_bytes, 64);
    assert!(graphics.texture_operations.lock().is_empty());
}

#[test]
fn test_clear_all_kitty_state_deflates_and_queues_once() {
    let mut graphics = Graphics::default();
    graphics.store_kitty_image(1, None, test_kitty_pixels(64));
    graphics.swap_kitty_screen_state();
    graphics.store_kitty_image(1, None, test_kitty_pixels(32));
    graphics.store_kitty_image(2, None, test_kitty_pixels(16));
    assert_eq!(graphics.total_bytes, 112);

    graphics.clear_all_kitty_state();

    assert_eq!(graphics.total_bytes, 0);
    assert!(graphics.kitty_images.is_empty());
    assert!(graphics.kitty_inactive_screen.kitty_images.is_empty());
    let ops = graphics.texture_operations.lock();
    let mut ids: Vec<u32> = ops
        .iter()
        .map(|removal| match removal {
            GraphicRemoval::Kitty(id) => *id,
            other => panic!("unexpected removal {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2], "each id queued exactly once across screens");
}

#[test]
fn test_eviction_of_shared_id_queues_single_removal() {
    let mut graphics = Graphics {
        total_limit: 100,
        ..Graphics::default()
    };
    graphics.store_kitty_image(1, None, test_kitty_pixels(60));
    graphics.swap_kitty_screen_state();
    graphics.store_kitty_image(1, None, test_kitty_pixels(40));
    assert_eq!(graphics.total_bytes, 100);

    // Freeing 80 bytes forces both copies out; the shared window-level
    // texture must be queued exactly once, after neither screen holds it.
    let used = std::collections::HashSet::new();
    assert!(graphics.evict_images(80, &used));

    assert_eq!(graphics.total_bytes, 0);
    assert_eq!(
        graphics.texture_operations.lock().as_slice(),
        &[GraphicRemoval::Kitty(1)]
    );
}

#[test]
fn test_over_budget_warn_latch_releases_below_budget() {
    let mut graphics = Graphics {
        total_limit: 100,
        ..Graphics::default()
    };
    graphics.store_kitty_image(1, None, test_kitty_pixels(80));
    graphics.over_budget_warned = true;

    graphics.delete_kitty_images(|_, _| true);

    assert_eq!(graphics.total_bytes, 0);
    assert!(!graphics.over_budget_warned);
}

#[test]
fn test_graphics_memory_tracking() {
    use sugarloaf::ColorType;
    let mut graphics = Graphics::default();

    // Create a small graphic (100x100 RGBA = 40,000 bytes)
    let pixels = vec![255u8; 100 * 100 * 4];
    let graphic = GraphicData {
        id: GraphicId::new(1),
        width: 100,
        height: 100,
        color_type: ColorType::Rgba,
        pixels,
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };

    let bytes = Graphics::calculate_graphic_bytes(&graphic);
    assert_eq!(bytes, 40_000);

    // Track the graphic
    graphics.track_graphic(GraphicId::new(1), bytes);
    assert_eq!(graphics.total_bytes, 40_000);
    assert!(graphics.image_timestamps.contains_key(&GraphicId::new(1)));

    // Untrack the graphic
    graphics.untrack_graphic(GraphicId::new(1), bytes);
    assert_eq!(graphics.total_bytes, 0);
    assert!(!graphics.image_timestamps.contains_key(&GraphicId::new(1)));
}

#[test]
fn test_graphics_eviction_unused_first() {
    use sugarloaf::ColorType;
    let mut graphics = Graphics {
        total_limit: 100_000, // 100KB limit for testing
        ..Graphics::default()
    };

    // Add 3 graphics (50KB each = 150KB total, will exceed limit)
    let mut used_ids = std::collections::HashSet::new();

    // Graphic 1: 50KB, used
    let pixels1 = vec![255u8; 50_000];
    let graphic1 = GraphicData {
        id: GraphicId::new(1),
        width: 100,
        height: 125,
        color_type: ColorType::Rgba,
        pixels: pixels1.clone(),
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };
    graphics.pending.push(graphic1);
    graphics.track_graphic(GraphicId::new(1), pixels1.len());
    used_ids.insert(1); // Mark as used

    std::thread::sleep(std::time::Duration::from_millis(10));

    // Graphic 2: 50KB, unused (should be evicted first)
    let pixels2 = vec![255u8; 50_000];
    let graphic2 = GraphicData {
        id: GraphicId::new(2),
        width: 100,
        height: 125,
        color_type: ColorType::Rgba,
        pixels: pixels2.clone(),
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };
    graphics.pending.push(graphic2);
    graphics.track_graphic(GraphicId::new(2), pixels2.len());
    // Not marked as used

    // Try to add Graphic 3 (will trigger eviction)
    let pixels3_len = 50_000;
    let success = graphics.evict_images(pixels3_len, &used_ids);

    assert!(success, "Eviction should succeed");
    // Graphic 2 (unused) should be evicted, Graphic 1 (used) should remain
    assert_eq!(graphics.pending.len(), 1);
    assert_eq!(graphics.pending[0].id, GraphicId::new(1));
    assert!(graphics.image_timestamps.contains_key(&GraphicId::new(1)));
    assert!(!graphics.image_timestamps.contains_key(&GraphicId::new(2)));
}

#[test]
fn test_graphics_eviction_oldest_first() {
    use sugarloaf::ColorType;
    let mut graphics = Graphics {
        total_limit: 100_000, // 100KB limit
        ..Graphics::default()
    };

    let used_ids = std::collections::HashSet::new(); // No images used

    // Add 3 graphics, all unused
    // Graphic 1: oldest
    let pixels1 = vec![255u8; 50_000];
    let graphic1 = GraphicData {
        id: GraphicId::new(1),
        width: 100,
        height: 125,
        color_type: ColorType::Rgba,
        pixels: pixels1.clone(),
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };
    graphics.pending.push(graphic1);
    graphics.track_graphic(GraphicId::new(1), pixels1.len());

    std::thread::sleep(std::time::Duration::from_millis(10));

    // Graphic 2: middle
    let pixels2 = vec![255u8; 50_000];
    let graphic2 = GraphicData {
        id: GraphicId::new(2),
        width: 100,
        height: 125,
        color_type: ColorType::Rgba,
        pixels: pixels2.clone(),
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };
    graphics.pending.push(graphic2);
    graphics.track_graphic(GraphicId::new(2), pixels2.len());

    // Try to add Graphic 3 (will trigger eviction, oldest should go first)
    let pixels3_len = 50_000;
    let success = graphics.evict_images(pixels3_len, &used_ids);

    assert!(success);
    // Graphic 1 (oldest) should be evicted
    assert_eq!(graphics.pending.len(), 1);
    assert_eq!(graphics.pending[0].id, GraphicId::new(2));
}

#[test]
fn test_graphics_eviction_fails_when_not_enough_space() {
    use sugarloaf::ColorType;
    let mut graphics = Graphics {
        total_limit: 100_000, // 100KB limit
        ..Graphics::default()
    };

    let mut used_ids = std::collections::HashSet::new();

    // Add one 90KB graphic that's in use
    let pixels1 = vec![255u8; 90_000];
    let graphic1 = GraphicData {
        id: GraphicId::new(1),
        width: 150,
        height: 150,
        color_type: ColorType::Rgba,
        pixels: pixels1.clone(),
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };
    graphics.pending.push(graphic1);
    graphics.track_graphic(GraphicId::new(1), pixels1.len());
    used_ids.insert(1); // Mark as used

    // Try to add another 90KB (total would be 180KB, exceeds limit)
    // Will evict the first one even though it's in use (per kitty spec)
    let pixels2_len = 90_000;
    let success = graphics.evict_images(pixels2_len, &used_ids);

    assert!(
        success,
        "Eviction should succeed by evicting used images if necessary"
    );
    // The used image should be evicted
    assert_eq!(graphics.pending.len(), 0);
}

#[test]
fn test_graphics_no_eviction_when_under_limit() {
    use sugarloaf::ColorType;
    let mut graphics = Graphics {
        total_limit: 200_000, // 200KB limit
        ..Graphics::default()
    };

    let used_ids = std::collections::HashSet::new();

    // Add one 50KB graphic
    let pixels1 = vec![255u8; 50_000];
    let graphic1 = GraphicData {
        id: GraphicId::new(1),
        width: 100,
        height: 125,
        color_type: ColorType::Rgba,
        pixels: pixels1.clone(),
        is_opaque: true,
        resize: None,
        display_width: None,
        display_height: None,
        transmit_time: std::time::Instant::now(),
    };
    graphics.pending.push(graphic1);
    graphics.track_graphic(GraphicId::new(1), pixels1.len());

    // Try to add another 50KB (total 100KB, well under limit)
    let pixels2_len = 50_000;
    let success = graphics.evict_images(pixels2_len, &used_ids);

    assert!(success);
    // No eviction should occur
    assert_eq!(graphics.pending.len(), 1);
    assert_eq!(graphics.total_bytes, 50_000);
}
