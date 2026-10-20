//! Feed a Sixel image and capture the decoded graphic via the event sink.
//!
//! Received images are drained into the terminal's graphics outbox and a
//! `RioEvent::UpdateGraphics` signal tells the renderer to take them, so
//! we install a small `EventListener` that counts the signals and then
//! take the outbox ourselves.
//!
//! Run: `cargo run -p rio-vt --example sixel`

use rio_vt::ansi::CursorShape;
use rio_vt::crosswords::{Crosswords, CrosswordsSize};
use rio_vt::event::{EventListener, RioEvent, WindowId};
use rio_vt::performer::handler::Processor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Counts how often the terminal announced graphics in its outbox.
#[derive(Clone, Default)]
struct GraphicSink(Arc<AtomicUsize>);

impl EventListener for GraphicSink {
    fn send_event(&self, event: RioEvent, _window: WindowId) {
        if let RioEvent::UpdateGraphics { .. } = event {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    // Without this the terminal drops the pixels and only signals.
    fn takes_graphics_outbox(&self) -> bool {
        true
    }
}

fn main() {
    let sink = GraphicSink::default();

    // Image protocols place graphics relative to the cell grid, so the
    // terminal needs real per-cell pixel dimensions (a renderer sets these;
    // here we pass them explicitly: 80x24 cells of 8x16 px).
    let size = CrosswordsSize::new_with_dimensions(80, 24, 640, 384, 8, 16);
    let mut term = Crosswords::new(
        size,
        CursorShape::Block,
        sink.clone(),
        WindowId::from(0),
        0,
        1_000,
    );

    // A real 64x64 Sixel image shipped as a test fixture. In a live
    // terminal these bytes arrive from the PTY.
    let sixel = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/sixel/testimage_im6.sixel"
    ))
    .expect("fixture present");

    let mut parser = Processor::default();
    parser.advance(&mut term, &sixel);

    println!("graphics signals: {}", sink.0.load(Ordering::Relaxed));
    match term.take_graphics_outbox() {
        None => println!("no sixel graphic captured"),
        Some(queues) => {
            for g in &queues.pending {
                println!(
                    "sixel graphic: id={} {}x{}px rgba_bytes={}",
                    g.id.get(),
                    g.width,
                    g.height,
                    g.pixels.len()
                );
            }
        }
    }
}
