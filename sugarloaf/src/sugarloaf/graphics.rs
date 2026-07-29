// Copyright (c) 2023-present, Raphael Amorim.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

// The graphic value types (GraphicData, GraphicId, ColorType, Resize*,
// GraphicOverlay, the image-key hashers) now live in the leaf crate
// `rio-graphics` so the terminal core can model images without pulling
// the renderer. They are re-exported here so `sugarloaf::graphics::*` and
// `sugarloaf::GraphicData` keep resolving. Only the render cache
// (GraphicDataEntry / Graphics), which owns a GPU-uploadable Handle,
// stays in sugarloaf.

use crate::sugarloaf::Handle;
use rustc_hash::FxHashMap;

pub use rio_graphics::{
    ColorType, Graphic, GraphicData, GraphicId, GraphicOverlay, ImageKey, ImageSource,
    ResizeCommand, ResizeParameter, MAX_GRAPHIC_DIMENSIONS,
};

pub struct GraphicDataEntry {
    pub handle: Handle,
    pub width: f32,
    pub height: f32,
    pub transmit_time: std::time::Instant,
}

impl GraphicDataEntry {
    /// Create from a GraphicData, taking ownership of pixel data.
    pub fn from_graphic_data(data: GraphicData) -> Self {
        let display_w = data.display_width.unwrap_or(data.width) as f32;
        let display_h = data.display_height.unwrap_or(data.height) as f32;
        Self {
            handle: Handle::from_pixels(
                data.width as u32,
                data.height as u32,
                data.pixels,
            ),
            width: display_w,
            height: display_h,
            transmit_time: data.transmit_time,
        }
    }
}

#[derive(Default)]
pub struct Graphics {
    inner: FxHashMap<GraphicId, GraphicDataEntry>,
}

impl Graphics {
    #[inline]
    pub fn get(&self, id: &GraphicId) -> Option<&GraphicDataEntry> {
        self.inner.get(id)
    }

    #[inline]
    pub fn insert(&mut self, graphic_data: GraphicData) {
        // Check if existing entry has the same generation (skip re-upload)
        if let Some(existing) = self.inner.get(&graphic_data.id) {
            if existing.transmit_time == graphic_data.transmit_time {
                return;
            }
        }

        let display_w = graphic_data.display_width.unwrap_or(graphic_data.width) as f32;
        let display_h = graphic_data.display_height.unwrap_or(graphic_data.height) as f32;
        self.inner.insert(
            graphic_data.id,
            GraphicDataEntry {
                handle: Handle::from_pixels(
                    graphic_data.width as u32,
                    graphic_data.height as u32,
                    graphic_data.pixels,
                ),
                width: display_w,
                height: display_h,
                transmit_time: graphic_data.transmit_time,
            },
        );
    }

    #[inline]
    pub fn remove(&mut self, graphic_id: &GraphicId) {
        self.inner.remove(graphic_id);
    }
}

#[test]
fn image_keys_discriminate_source_and_owner() {
    // Implicit kitty ids allocate from 0x80000000 while the first sixel's
    // GraphicId is 1 — under the old flag-packed u32 key both mapped to
    // 0x80000001 and the kitty transmit clobbered the sixel's entry.
    assert_ne!(ImageKey::kitty(7, 0x8000_0001), ImageKey::atlas(7, 1));
    // A client-supplied kitty i= may occupy any u32 value, including ones
    // numerically equal to an atlas GraphicId.
    assert_ne!(ImageKey::kitty(7, 1), ImageKey::atlas(7, 1));
    // Per-terminal id counters restart at 1 (atlas) / 0x80000000 (kitty),
    // so the same protocol id from two tabs sharing a window must map to
    // distinct entries.
    assert_ne!(ImageKey::atlas(1, 1), ImageKey::atlas(2, 1));
    assert_ne!(
        ImageKey::kitty(1, 0x8000_0000),
        ImageKey::kitty(2, 0x8000_0000)
    );
    // Identity still holds within one terminal + source.
    assert_eq!(ImageKey::kitty(3, 42), ImageKey::kitty(3, 42));
    assert_eq!(ImageKey::atlas(3, 42), ImageKey::atlas(3, 42));
}

#[test]
fn check_opaque_region() {
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
