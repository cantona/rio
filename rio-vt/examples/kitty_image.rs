//! Transmit an image with the Kitty graphics protocol and capture it.
//!
//! Kitty images wait in the terminal's graphics outbox in the
//! `pending_images` queue (keyed by the client's image id), separate from
//! the sixel/iTerm2 `pending` queue; `RioEvent::UpdateGraphics` says that
//! the outbox has something to take.
//!
//! Run: `cargo run -p rio-vt --example kitty_image`

use rio_vt::ansi::CursorShape;
use rio_vt::crosswords::{Crosswords, CrosswordsSize};
use rio_vt::event::{EventListener, RioEvent, WindowId};
use rio_vt::performer::handler::Processor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Counts how often the terminal announced graphics in its outbox.
#[derive(Clone, Default)]
struct ImageSink(Arc<AtomicUsize>);

impl EventListener for ImageSink {
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
    let sink = ImageSink::default();

    let size = CrosswordsSize::new_with_dimensions(80, 24, 640, 384, 8, 16);
    let mut term = Crosswords::new(
        size,
        CursorShape::Block,
        sink.clone(),
        WindowId::from(0),
        0,
        1_000,
    );

    // A 2x2 solid-red RGBA image, base64-encoded, transmitted with the
    // Kitty graphics protocol:
    //   f=32  -> 32-bit RGBA pixels
    //   s,v   -> source width/height in pixels
    //   a=T   -> transmit AND display
    // The payload is `\x1b_G<control data>;<base64 pixels>\x1b\\`.
    let kitty = b"\x1b_Gf=32,s=2,v=2,a=T;/wAA//8AAP//AAD//wAA/w==\x1b\\";

    let mut parser = Processor::default();
    parser.advance(&mut term, kitty);

    println!("graphics signals: {}", sink.0.load(Ordering::Relaxed));
    match term.take_graphics_outbox() {
        None => println!("no kitty image captured"),
        Some(queues) => {
            for (id, g) in &queues.pending_images {
                println!(
                    "kitty image: id={id} {}x{}px rgba_bytes={}",
                    g.width,
                    g.height,
                    g.pixels.len()
                );
            }
        }
    }
}
