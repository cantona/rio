use rio_backend::sugarloaf::text::DrawOpts;
use rio_backend::sugarloaf::Sugarloaf;

const CONFIRM: &str = "yes (y)";
const DISMISS: &str = "no (n)";

/// What a confirmed "y" should close: a specific tab (close button),
/// the current split-or-tab (keyboard shortcut / palette), or — when
/// the last tab is closing — the whole window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PendingClose {
    Tab(usize),
    Current,
    Window,
}

/// `[navigation] tab-close-confirm = "ask"` overlay: gates a tab close
/// behind a y/n answer. Same shape as `ConfirmQuit`.
#[derive(Default)]
pub struct ConfirmClose {
    pending: Option<PendingClose>,
}

impl ConfirmClose {
    #[inline]
    pub fn is_active(&self) -> bool {
        self.pending.is_some()
    }

    #[inline]
    pub fn pending(&self) -> Option<PendingClose> {
        self.pending
    }

    #[inline]
    pub fn set_pending(&mut self, pending: Option<PendingClose>) {
        self.pending = pending;
    }

    /// `dimensions` is `(window_width, window_height, scale_factor)`,
    /// matching the other overlays' `render` signature.
    pub fn render(&self, sugarloaf: &mut Sugarloaf, dimensions: (f32, f32, f32)) {
        let Some(pending) = self.pending else {
            return;
        };
        let heading = match pending {
            PendingClose::Window => "close this window?",
            _ => "close this tab?",
        };

        let (width, height, scale) = dimensions;
        let win_w = width / scale;
        let win_h = height / scale;

        let full_text = format!("{heading}  {CONFIRM}  /  {DISMISS}");
        let padding_x = 12.0;
        let padding_y = 6.0;
        let text_h = 16.0;
        let box_w = full_text.len() as f32 * 7.5 + padding_x * 2.0;
        let box_h = text_h + padding_y * 2.0;
        let box_x = (win_w - box_w) / 2.0;
        let box_y = (win_h - box_h) / 2.0;

        sugarloaf.rect(
            None,
            box_x,
            box_y,
            box_w,
            box_h,
            [0.0, 0.0, 0.0, 1.0],
            0.0,
            20,
        );

        let heading_opts = DrawOpts {
            font_size: 13.0,
            color: [255, 255, 255, 255],
            ..DrawOpts::default()
        };
        let gray_opts = DrawOpts {
            font_size: 13.0,
            color: [166, 166, 166, 255],
            ..DrawOpts::default()
        };

        let text_x = box_x + padding_x;
        let text_y = box_y + padding_y + 2.0;

        let ui = sugarloaf.text_mut();
        let heading_w = ui.draw(text_x, text_y, heading, &heading_opts);
        ui.draw(
            text_x + heading_w,
            text_y,
            &format!("  {CONFIRM}  /  {DISMISS}"),
            &gray_opts,
        );
    }
}
