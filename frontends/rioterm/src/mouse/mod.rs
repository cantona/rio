// encode_mouse_report is adapted from sgr_mouse_report and normal_mouse_report in
// https://github.com/alacritty/alacritty/blob/c39c3c97f1a1213418c3629cc59a1d46e34070e0/alacritty/src/input.rs
// which is licensed under Apache 2.0 license.

use crate::crosswords::pos::Column;
use crate::crosswords::pos::Line;
use crate::crosswords::pos::Side;
use crate::crosswords::Mode;
use crate::event::ClickState;
use rio_backend::crosswords::pos::Pos;
use rio_window::event::ElementState;
use rio_window::event::MouseButton;
use std::time::Instant;

#[derive(Default, Debug)]
pub struct AccumulatedScroll {
    /// Scroll we should perform along `x` axis.
    pub x: f64,

    /// Scroll we should perform along `y` axis.
    pub y: f64,
}

#[derive(Debug)]
pub struct Mouse {
    pub multiplier: f64,
    pub divider: f64,
    pub left_button_state: ElementState,
    pub middle_button_state: ElementState,
    pub right_button_state: ElementState,
    pub last_click_timestamp: Instant,
    pub last_click_button: MouseButton,
    pub click_state: ClickState,
    pub accumulated_scroll: AccumulatedScroll,
    pub square_side: Side,
    pub inside_text_area: bool,
    /// Cursor X in physical pixels. `f64` so subpixel precision from
    /// the OS event survives all the way to the cell-grid divide.
    pub x: f64,
    /// Cursor Y in physical pixels.
    pub y: f64,
    pub on_border: bool,
    /// Raw (unclamped) cursor Y in physical pixels, for selection scroll.
    pub raw_y: f64,
    /// Last cell (line, column) the cursor was over. `None` until the
    /// first `CursorMoved` event arrives. Used by the input dispatcher
    /// to skip hint / OSC-8 / hyperlink work when the cursor moves
    /// within the same cell — replaces the old pixel-equality check
    /// that fired on every subpixel HiDPI jitter.
    pub last_cell: Option<Pos>,
    /// Last application-grid pixel the cursor was over, as SGR-Pixels
    /// reports it.
    pub last_pixel: Option<(usize, usize)>,
    /// The hint under an in-flight left press that bypassed mouse
    /// reporting. The release consults this latch instead of
    /// re-checking the highlight (the modifier can change mid-click,
    /// and re-evaluating would let an application see a press without
    /// its release) and follows the link only when the release lands
    /// on the same hint.
    pub hint_click_latched: Option<crate::hints::HintMatch>,
}

impl Default for Mouse {
    fn default() -> Mouse {
        Mouse {
            multiplier: 3.0,
            divider: 1.0,
            last_click_timestamp: Instant::now(),
            last_click_button: MouseButton::Left,
            left_button_state: ElementState::Released,
            middle_button_state: ElementState::Released,
            right_button_state: ElementState::Released,
            click_state: ClickState::None,
            square_side: Side::Left,
            inside_text_area: Default::default(),
            on_border: false,
            accumulated_scroll: AccumulatedScroll::default(),
            x: 0.0,
            y: 0.0,
            raw_y: 0.0,
            last_cell: None,
            last_pixel: None,
            hint_click_latched: None,
        }
    }
}

impl Mouse {
    pub fn new(multiplier: f64, divider: f64) -> Self {
        Self {
            multiplier,
            divider,
            ..Default::default()
        }
    }

    #[inline]
    pub fn set_multiplier_and_divider(&mut self, multiplier: f64, divider: f64) {
        self.multiplier = multiplier;
        self.divider = divider;
    }

    /// Record the grid position under the cursor and say what changed.
    #[inline]
    pub fn track_position(&mut self, position: &GridPosition, mode: Mode) -> PointerMove {
        let cell_changed = self.last_cell != Some(position.cell);
        let pixel_changed = self.last_pixel != Some(position.pixel);
        self.last_cell = Some(position.cell);
        self.last_pixel = Some(position.pixel);

        PointerMove {
            cell_changed,
            pixel_motion: pixel_changed && mode.contains(Mode::SGR_PIXELS_MOUSE),
        }
    }
}

/// Whether `mode` asks for a cursor move to be reported: any move under 1003,
/// and under 1002 only while a button is held.
#[inline]
pub fn motion_is_reported(mode: Mode, button_held: bool) -> bool {
    mode.contains(Mode::MOUSE_MOTION) || (button_held && mode.contains(Mode::MOUSE_DRAG))
}

/// What a cursor move changed, from [`Mouse::track_position`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerMove {
    /// The cursor is over a different cell.
    pub cell_changed: bool,
    /// The reported pixel changed while SGR-Pixels (1016) is set, so the
    /// move is motion to report even within one cell.
    pub pixel_motion: bool,
}

/// A cursor position resolved against a terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPosition {
    /// The cell under the cursor, viewport-relative.
    pub cell: Pos,
    /// The cursor in pixels from the top-left of the application's grid,
    /// 1-based and clamped to `columns * cell_width` by
    /// `(rows - display_offset) * cell_height`, as SGR-Pixels (mode 1016)
    /// reports it.
    pub pixel: (usize, usize),
}

/// Map a physical-pixel cursor position to the grid cell under it and to its
/// pixel offset in the application's grid, from one origin so the two cannot
/// disagree. Both shift by `display_offset` when the viewport is scrolled
/// back.
///
/// Pixel coords stay `f64` until the final integer truncation, and
/// the divide uses the canonical `u32` `cell_width / cell_height`
/// (the same integers the GPU shader paints with — no drift between
/// painted cell stride and click→cell mapping).
///
/// `margin_x_left / margin_y_top` are already pre-scaled (physical
/// pixels), do not multiply by `scale_factor` here. For a split they include
/// the panel's origin in the window.
#[inline]
pub fn calculate_grid_position(
    mouse: &Mouse,
    display_offset: usize,
    columns_rows: (usize, usize),
    margin_x_left: f32,
    margin_y_top: f32,
    cell: (u32, u32),
) -> GridPosition {
    let (cell_w, cell_h) = (cell.0 as f64, cell.1 as f64);
    if cell_w == 0.0 || cell_h == 0.0 {
        return GridPosition {
            cell: Pos::default(),
            pixel: (1, 1),
        };
    }

    let margin_x = margin_x_left as f64;
    let margin_y = margin_y_top as f64;

    // f64 throughout. Negative-clamp via `.max(0.0)` so clicks in
    // the margin map to col/row 0 rather than wrapping or
    // overflowing on the cast.
    let x_in_grid = (mouse.x - margin_x).max(0.0);
    let y_in_grid = (mouse.y - margin_y).max(0.0);
    let col_idx = (x_in_grid / cell_w) as usize;
    let row_idx = (y_in_grid / cell_h) as usize;

    let col = std::cmp::min(Column(col_idx), Column(columns_rows.0 - 1));
    let row = std::cmp::min(row_idx, columns_rows.1 - 1);
    let row = Line(row as i32) - display_offset;

    let grid_w = columns_rows.0 * cell.0 as usize;
    // Pixels are relative to the application's grid, like the cell row: a
    // viewport scrolled back by `display_offset` lines shows the
    // application's first row that many rows further down, and that many
    // of its last rows are off screen.
    let grid_h = columns_rows.1.saturating_sub(display_offset) * cell.1 as usize;
    let y_in_app = y_in_grid - display_offset as f64 * cell_h;
    let pixel_x = (x_in_grid as usize + 1).min(grid_w).max(1);
    let pixel_y = (y_in_app.max(0.0) as usize + 1).min(grid_h).max(1);

    GridPosition {
        cell: Pos::new(row, col),
        pixel: (pixel_x, pixel_y),
    }
}

/// Encode a mouse report in the encoding `mode` selects: SGR-Pixels (1016),
/// SGR (1006), or the default byte encoding with its UTF-8 extension (1005).
///
/// `button` is the button code without modifiers and `mods` the modifier
/// bits. Returns `None` when the cell cannot be represented in the default
/// encoding, or lies in the scrollback.
pub fn encode_mouse_report(
    mode: Mode,
    position: &GridPosition,
    button: u8,
    mods: u8,
    state: ElementState,
) -> Option<Vec<u8>> {
    let sgr = |x: usize, y: usize| {
        let c = match state {
            ElementState::Pressed => 'M',
            ElementState::Released => 'm',
        };
        format!("\x1b[<{};{};{}{}", button + mods, x, y, c).into_bytes()
    };

    let col = position.cell.col.0;
    let row = usize::try_from(position.cell.row.0).ok()?;

    if mode.contains(Mode::SGR_PIXELS_MOUSE) {
        let (x, y) = position.pixel;
        return Some(sgr(x, y));
    }
    if mode.contains(Mode::SGR_MOUSE) {
        return Some(sgr(col + 1, row + 1));
    }

    let utf8 = mode.contains(Mode::UTF8_MOUSE);
    let max_point = if utf8 { 2015 } else { 223 };
    if row >= max_point || col >= max_point {
        return None;
    }

    let code = match state {
        ElementState::Pressed => button + mods,
        ElementState::Released => 3 + mods,
    };
    let mut msg = vec![b'\x1b', b'[', b'M', 32 + code];

    let mut push_pos = |pos: usize| {
        if utf8 && pos >= 95 {
            let pos = 32 + 1 + pos;
            msg.push((0xC0 + pos / 64) as u8);
            msg.push((0x80 + (pos & 63)) as u8);
        } else {
            msg.push(32 + 1 + pos as u8);
        }
    };
    push_pos(col);
    push_pos(row);

    Some(msg)
}

/// Determine which side of a cell the mouse x-position falls on.
///
/// `margin_x` is pre-scaled (physical pixels). `cell_width` is the
/// canonical `u32` cell width (same value the renderer paints with).
/// `grid_width` is the total width of the grid area in physical
/// pixels.
///
/// 60% threshold rather than the 50% midpoint inherited from
/// alacritty: clicks land on a cell until the cursor is past 60%
/// across it, and a drag must cross 60% of the next cell before
/// it's included — reduces accidental half-cell snapping at the
/// midpoint.
#[inline]
pub fn calculate_side_by_pos(
    x: f64,
    margin_x: f32,
    cell_width: u32,
    grid_width: f32,
) -> Side {
    let cell_w = cell_width as f64;
    let margin = margin_x as f64;
    let grid_w = grid_width as f64;

    let x_in_grid = (x - margin).max(0.0);
    let cell_x = x_in_grid % cell_w;
    let threshold = cell_w * 0.6;

    let usable = (grid_w - margin).max(0.0);
    let additional_padding = usable % cell_w;
    let end_of_grid = margin + usable - additional_padding;

    if cell_x >= threshold || x >= end_of_grid {
        Side::Right
    } else {
        Side::Left
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    fn calculate_mouse_position(
        mouse: &Mouse,
        display_offset: usize,
        columns_rows: (usize, usize),
        margin_x_left: f32,
        margin_y_top: f32,
        cell: (u32, u32),
    ) -> Pos {
        calculate_grid_position(
            mouse,
            display_offset,
            columns_rows,
            margin_x_left,
            margin_y_top,
            cell,
        )
        .cell
    }

    fn mk_mouse(x: f64, y: f64) -> Mouse {
        Mouse {
            x,
            y,
            ..Default::default()
        }
    }

    /// Canonical stride: cell width = 9 (u32). Boundaries at 0, 9, 18, 27.
    #[test]
    fn pos_calc_basic_no_margin() {
        let cols = 10;
        let lines = 5;
        let cell = (9u32, 18u32);

        // x=8 → 8/9 = 0 → col 0
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(8.0, 0.0),
                0,
                (cols, lines),
                0.0,
                0.0,
                cell
            )
            .col,
            Column(0),
        );
        // x=8.99 → still col 0 (subpixel precision preserved)
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(8.99, 0.0),
                0,
                (cols, lines),
                0.0,
                0.0,
                cell
            )
            .col,
            Column(0),
        );
        // x=9 → boundary → col 1
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(9.0, 0.0),
                0,
                (cols, lines),
                0.0,
                0.0,
                cell
            )
            .col,
            Column(1),
        );
        // x=17.5 → 17.5/9 = 1.94 → col 1
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(17.5, 0.0),
                0,
                (cols, lines),
                0.0,
                0.0,
                cell
            )
            .col,
            Column(1),
        );
        // x=18 → col 2
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(18.0, 0.0),
                0,
                (cols, lines),
                0.0,
                0.0,
                cell
            )
            .col,
            Column(2),
        );
    }

    /// Pre-scaled margin: clicks before the margin clamp to col 0.
    /// Boundaries at 10, 19, 28, 37, ... (cell width 9 with margin 10).
    #[test]
    fn pos_calc_with_prescaled_margin() {
        let cols = 10;
        let lines = 5;
        let cell = (9u32, 18u32);
        let margin_x = 10.0_f32;

        // Click before margin → col 0.
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(5.0, 0.0),
                0,
                (cols, lines),
                margin_x,
                0.0,
                cell
            )
            .col,
            Column(0),
        );
        // x=10 → col 0 (start of cell 0).
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(10.0, 0.0),
                0,
                (cols, lines),
                margin_x,
                0.0,
                cell
            )
            .col,
            Column(0),
        );
        // x=18.99 → still col 0.
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(18.99, 0.0),
                0,
                (cols, lines),
                margin_x,
                0.0,
                cell
            )
            .col,
            Column(0),
        );
        // x=19 → col 1.
        assert_eq!(
            calculate_mouse_position(
                &mk_mouse(19.0, 0.0),
                0,
                (cols, lines),
                margin_x,
                0.0,
                cell
            )
            .col,
            Column(1),
        );
    }

    /// Regression for the actual mouse-positioning bug: with the
    /// renderer painting at canonical `u32` stride `cell.cell_width`,
    /// a click on the visual middle of a high-index column must map
    /// to that same column index. The old code passed unrounded
    /// `f32` cell_width to the divide, causing accumulating drift
    /// (≈0.41px per col → wrong by 3 columns at col 100 with width
    /// 16.41).
    #[test]
    fn pos_calc_no_drift_at_high_column_index() {
        let cols = 200;
        let lines = 50;
        // u32 stride — the same value the GPU shader paints with.
        let cell = (16u32, 33u32);

        // Painted col 100 occupies pixels [1600, 1616). Click in the middle.
        let pos = calculate_mouse_position(
            &mk_mouse(1608.0, 100.0),
            0,
            (cols, lines),
            0.0,
            0.0,
            cell,
        );
        assert_eq!(pos.col, Column(100));

        // Painted col 150 at pixels [2400, 2416). Click left edge.
        let pos = calculate_mouse_position(
            &mk_mouse(2400.0, 100.0),
            0,
            (cols, lines),
            0.0,
            0.0,
            cell,
        );
        assert_eq!(pos.col, Column(150));
    }

    /// Subpixel mouse precision survives the divide. With cell=16
    /// and HiDPI delivering `1608.5` from the OS, the f64 path gives
    /// col 100; if we'd cast `mouse.x` to `usize` early (the old
    /// behavior) we'd see `1608 / 16 = 100` too, but at the boundary
    /// (e.g. `1599.9 → col 99` not `col 100`) precision matters.
    #[test]
    fn pos_calc_subpixel_precision_preserved() {
        let cell = (16u32, 33u32);
        let cols = 200;
        let lines = 50;

        // 1599.9 should map to col 99 (cell 99 at [1584,1600)).
        let pos = calculate_mouse_position(
            &mk_mouse(1599.9, 100.0),
            0,
            (cols, lines),
            0.0,
            0.0,
            cell,
        );
        assert_eq!(pos.col, Column(99));

        // 1600.1 should map to col 100.
        let pos = calculate_mouse_position(
            &mk_mouse(1600.1, 100.0),
            0,
            (cols, lines),
            0.0,
            0.0,
            cell,
        );
        assert_eq!(pos.col, Column(100));
    }

    /// Y axis: pre-scaled margin must not be re-scaled. Row stride
    /// is the canonical `cell.1` (already includes line_height).
    #[test]
    fn pos_calc_row_with_prescaled_margin() {
        let cols = 96;
        let lines = 27;
        let margin_y = 72.0_f32;
        let cell = (16u32, 33u32);

        // Row 0 begins at y = 72. Row 6 spans [72+198, 72+231) = [270, 303).
        let pos = calculate_mouse_position(
            &mk_mouse(100.0, 280.0),
            0,
            (cols, lines),
            0.0,
            margin_y,
            cell,
        );
        assert_eq!(pos.row, Line(6));

        // Row 22 spans [72 + 22*33, 72 + 23*33) = [798, 831).
        let pos = calculate_mouse_position(
            &mk_mouse(100.0, 820.0),
            0,
            (cols, lines),
            0.0,
            margin_y,
            cell,
        );
        assert_eq!(pos.row, Line(22));
    }

    /// Display offset shifts the reported row by the scrollback
    /// position so callers get a viewport-relative `Line` index.
    #[test]
    fn pos_calc_display_offset_shifts_row() {
        let cell = (16u32, 33u32);
        let cols = 96;
        let lines = 27;

        // Row index 5 from top, display_offset 10 → Line(5) - 10 = Line(-5).
        let pos = calculate_mouse_position(
            &mk_mouse(100.0, 5.0 * 33.0 + 1.0),
            10,
            (cols, lines),
            0.0,
            0.0,
            cell,
        );
        assert_eq!(pos.row, Line(-5));
    }

    /// 60% threshold: cell_x < 0.6*cell_w → Left, otherwise Right.
    /// Boundary lands on subpixel position.
    #[test]
    fn side_by_pos_60_percent_threshold() {
        let cell = 16u32;
        let margin_x = 0.0_f32;
        let grid_w = 16.0 * 200.0;

        // cell_x = 9.59 < 9.6 → Left.
        assert_eq!(
            calculate_side_by_pos(9.59, margin_x, cell, grid_w),
            Side::Left,
        );
        // cell_x ≥ 9.6 → Right. Use 9.61 to dodge f64 rep of 9.6.
        assert_eq!(
            calculate_side_by_pos(9.61, margin_x, cell, grid_w),
            Side::Right,
        );
    }

    /// Regression: side detection had drift at high columns when the
    /// f32 divide was off by half a pixel per cell. With u32 stride
    /// the modulo stays exact at any column.
    #[test]
    fn side_by_pos_no_drift_at_high_columns() {
        let cell = 16u32;
        let margin_x = 0.0_f32;
        let grid_w = 16.0 * 200.0;

        // Col 100 = pixel [1600, 1616). Left side: cell_x = 0..9.6.
        assert_eq!(
            calculate_side_by_pos(1600.0, margin_x, cell, grid_w),
            Side::Left,
        );
        assert_eq!(
            calculate_side_by_pos(1609.5, margin_x, cell, grid_w),
            Side::Left,
        );
        // ≥ 9.6 → Right. Use 9.61 to dodge f64 representation of 9.6.
        assert_eq!(
            calculate_side_by_pos(1609.61, margin_x, cell, grid_w),
            Side::Right,
        );
    }

    /// Margin pre-scaled, must not be re-scaled in the side
    /// calculation. Pixel before margin clamps to Left.
    #[test]
    fn side_by_pos_prescaled_margin() {
        let cell = 16u32;
        let margin_x = 40.0_f32;
        let grid_w = 40.0 + 80.0 * 16.0;

        // Before margin → Left.
        assert_eq!(
            calculate_side_by_pos(30.0, margin_x, cell, grid_w),
            Side::Left,
        );
        // x=49: cell_x = 9 < 9.6 → Left.
        assert_eq!(
            calculate_side_by_pos(49.0, margin_x, cell, grid_w),
            Side::Left,
        );
        // x=49.61 → cell_x ≥ 9.6 → Right (49.6 hits f64 rep edge).
        assert_eq!(
            calculate_side_by_pos(49.61, margin_x, cell, grid_w),
            Side::Right,
        );
    }

    fn grid_pos(x: f64, y: f64, margin: (f32, f32), cell: (u32, u32)) -> GridPosition {
        calculate_grid_position(&mk_mouse(x, y), 0, (10, 5), margin.0, margin.1, cell)
    }

    #[test]
    fn grid_pos_pixel_is_one_based_from_the_margin() {
        let cell = (9u32, 18u32);
        let margin = (10.0_f32, 20.0_f32);

        let pos = grid_pos(10.0, 20.0, margin, cell);
        assert_eq!(pos.pixel, (1, 1));
        assert_eq!(pos.cell, Pos::new(Line(0), Column(0)));

        let pos = grid_pos(19.5, 38.0, margin, cell);
        assert_eq!(pos.pixel, (10, 19));
        assert_eq!(pos.cell, Pos::new(Line(1), Column(1)));
    }

    #[test]
    fn grid_pos_last_pixel_of_last_cell() {
        // 10 columns of 9 and 5 rows of 18: the text area is 90x90.
        let cell = (9u32, 18u32);

        let pos = grid_pos(89.9, 89.9, (0.0, 0.0), cell);
        assert_eq!(pos.pixel, (90, 90));
        assert_eq!(pos.cell, Pos::new(Line(4), Column(9)));
    }

    #[test]
    fn grid_pos_pixel_clamps_in_the_margins() {
        let cell = (9u32, 18u32);
        let margin = (10.0_f32, 20.0_f32);

        let pos = grid_pos(3.0, 5.0, margin, cell);
        assert_eq!(pos.pixel, (1, 1));
        assert_eq!(pos.cell, Pos::new(Line(0), Column(0)));

        // Right and bottom padding past the last cell.
        let pos = grid_pos(10.0 + 90.0 + 7.0, 20.0 + 90.0 + 12.0, margin, cell);
        assert_eq!(pos.pixel, (90, 90));
        assert_eq!(pos.cell, Pos::new(Line(4), Column(9)));
    }

    #[test]
    fn grid_pos_fractional_hidpi_scale() {
        // Scale 1.5: a 5px margin is 7.5 physical pixels, and the cursor
        // arrives at fractional physical positions.
        let cell = (14u32, 27u32);
        let margin = (7.5_f32, 7.5_f32);

        let pos = grid_pos(7.5 + 20.25, 7.5 + 53.9, margin, cell);
        assert_eq!(pos.pixel, (21, 54));
        assert_eq!(pos.cell, Pos::new(Line(1), Column(1)));

        let pos = grid_pos(7.5 + 13.99, 7.5 + 26.99, margin, cell);
        assert_eq!(pos.pixel, (14, 27));
        assert_eq!(pos.cell, Pos::new(Line(0), Column(0)));
    }

    #[test]
    fn grid_pos_split_with_non_zero_origin() {
        // A right-hand split at x = 400, y = 0: the margin the layout hands
        // over is the window margin plus the panel's origin.
        let cell = (9u32, 18u32);
        let margin = (10.0_f32 + 400.0, 20.0_f32);

        let pos = grid_pos(415.0, 25.0, margin, cell);
        assert_eq!(pos.pixel, (6, 6));
        assert_eq!(pos.cell, Pos::new(Line(0), Column(0)));

        // On the divider, left of the panel's text area.
        let pos = grid_pos(405.0, 25.0, margin, cell);
        assert_eq!(pos.pixel, (1, 6));
    }

    #[test]
    fn grid_pos_pixel_follows_display_offset() {
        let cell = (9u32, 18u32);
        // Scrolled back one line: viewport row 2 is the application's row 1,
        // and the pixel moves up by one cell with it.
        let pos =
            calculate_grid_position(&mk_mouse(40.0, 40.0), 1, (10, 5), 0.0, 0.0, cell);
        assert_eq!(pos.cell.row, Line(2) - 1);
        assert_eq!(pos.pixel, (41, 23));
        // Below the grid: the last visible application row is lines-1-offset,
        // so the pixel clamps to the bottom of that row, like the cell.
        let pos =
            calculate_grid_position(&mk_mouse(40.0, 100.0), 1, (10, 5), 0.0, 0.0, cell);
        assert_eq!(pos.cell.row, Line(3));
        assert_eq!(pos.pixel, (41, 72));
        // Scrolled back past the pointer: the cell is in the scrollback (the
        // encoder drops it) and the pixel clamps to the first row.
        let pos =
            calculate_grid_position(&mk_mouse(40.0, 40.0), 3, (10, 5), 0.0, 0.0, cell);
        assert_eq!(pos.cell.row, Line(2) - 3);
        assert_eq!(pos.pixel, (41, 1));
    }

    fn at(col: usize, row: i32, pixel: (usize, usize)) -> GridPosition {
        GridPosition {
            cell: Pos::new(Line(row), Column(col)),
            pixel,
        }
    }

    fn encode(mode: Mode, pos: &GridPosition, button: u8, state: ElementState) -> String {
        let bytes = encode_mouse_report(mode, pos, button, 0, state).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn encode_sgr_reports_cells() {
        let pos = at(2, 3, (25, 60));
        assert_eq!(
            encode(Mode::SGR_MOUSE, &pos, 0, ElementState::Pressed),
            "\x1b[<0;3;4M"
        );
        assert_eq!(
            encode(Mode::SGR_MOUSE, &pos, 0, ElementState::Released),
            "\x1b[<0;3;4m"
        );
    }

    #[test]
    fn encode_sgr_pixels_reports_pixels() {
        let pos = at(2, 3, (25, 60));
        assert_eq!(
            encode(Mode::SGR_PIXELS_MOUSE, &pos, 0, ElementState::Pressed),
            "\x1b[<0;25;60M"
        );
        assert_eq!(
            encode(Mode::SGR_PIXELS_MOUSE, &pos, 2, ElementState::Released),
            "\x1b[<2;25;60m"
        );
        // Motion with the left button held, and the wheel.
        assert_eq!(
            encode(Mode::SGR_PIXELS_MOUSE, &pos, 32, ElementState::Pressed),
            "\x1b[<32;25;60M"
        );
        assert_eq!(
            encode(Mode::SGR_PIXELS_MOUSE, &pos, 65, ElementState::Pressed),
            "\x1b[<65;25;60M"
        );
        let ctrl_wheel = encode_mouse_report(
            Mode::SGR_PIXELS_MOUSE,
            &pos,
            64,
            16,
            ElementState::Pressed,
        );
        assert_eq!(ctrl_wheel.unwrap(), b"\x1b[<80;25;60M");
    }

    #[test]
    fn encode_default_and_utf8() {
        let pos = at(2, 3, (25, 60));
        let mode = Mode::empty();
        assert_eq!(
            encode_mouse_report(mode, &pos, 0, 0, ElementState::Pressed).unwrap(),
            vec![0x1b, b'[', b'M', 32, 35, 36]
        );
        assert_eq!(
            encode_mouse_report(mode, &pos, 0, 4, ElementState::Released).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 3 + 4, 35, 36]
        );

        let far = at(300, 3, (2701, 60));
        assert_eq!(
            encode_mouse_report(mode, &far, 0, 0, ElementState::Pressed),
            None
        );
        let pos = 32 + 1 + 300;
        assert_eq!(
            encode_mouse_report(Mode::UTF8_MOUSE, &far, 0, 0, ElementState::Pressed)
                .unwrap(),
            vec![
                0x1b,
                b'[',
                b'M',
                32,
                (0xC0 + pos / 64) as u8,
                (0x80 + (pos & 63)) as u8,
                36
            ]
        );
    }

    #[test]
    fn encode_skips_scrollback_rows() {
        let pos = at(2, -1, (25, 60));
        for mode in [Mode::empty(), Mode::SGR_MOUSE, Mode::SGR_PIXELS_MOUSE] {
            assert_eq!(
                encode_mouse_report(mode, &pos, 0, 0, ElementState::Pressed),
                None
            );
        }
    }

    /// The encoding follows the terminal's mode through 1006 and 1016 being
    /// set and reset the way terminal-browser and herdr do it.
    #[test]
    fn encode_follows_mode_1016_set_and_reset() {
        use rio_backend::ansi::CursorShape;
        use rio_backend::crosswords::{Crosswords, CrosswordsSize};
        use rio_backend::event::{VoidListener, WindowId};
        use rio_backend::performer::handler::Processor;

        let mut term = Crosswords::new(
            CrosswordsSize::new(10, 5),
            CursorShape::Block,
            VoidListener {},
            WindowId::from(0),
            0,
            10,
        );
        let mut parser = Processor::default();
        let pos = at(2, 3, (25, 60));
        let press = |term: &Crosswords<VoidListener>| {
            encode_mouse_report(term.mode(), &pos, 0, 0, ElementState::Pressed).unwrap()
        };

        parser.advance(&mut term, b"\x1b[?1003h\x1b[?1006h");
        assert_eq!(press(&term), b"\x1b[<0;3;4M");

        parser.advance(&mut term, b"\x1b[?1016h");
        assert_eq!(press(&term), b"\x1b[<0;25;60M");

        // 1016 cleared 1006, so resetting it leaves the default encoding.
        parser.advance(&mut term, b"\x1b[?1016l");
        assert_eq!(press(&term), vec![0x1b, b'[', b'M', 32, 35, 36]);
    }

    #[test]
    fn move_within_a_cell_is_motion_only_under_sgr_pixels() {
        let first = at(2, 3, (25, 60));
        let same_cell = at(2, 3, (26, 61));
        let next_cell = at(3, 3, (28, 61));

        let mut mouse = Mouse::default();
        mouse.track_position(&first, Mode::SGR_MOUSE);
        assert_eq!(
            mouse.track_position(&same_cell, Mode::SGR_MOUSE),
            PointerMove {
                cell_changed: false,
                pixel_motion: false,
            }
        );
        assert_eq!(
            mouse.track_position(&next_cell, Mode::SGR_MOUSE),
            PointerMove {
                cell_changed: true,
                pixel_motion: false,
            }
        );

        let mut mouse = Mouse::default();
        mouse.track_position(&first, Mode::SGR_PIXELS_MOUSE);
        assert_eq!(
            mouse.track_position(&same_cell, Mode::SGR_PIXELS_MOUSE),
            PointerMove {
                cell_changed: false,
                pixel_motion: true,
            }
        );
        // Subpixel jitter that lands on the same integer pixel is not motion.
        assert_eq!(
            mouse.track_position(&same_cell, Mode::SGR_PIXELS_MOUSE),
            PointerMove {
                cell_changed: false,
                pixel_motion: false,
            }
        );
        assert_eq!(
            mouse.track_position(&next_cell, Mode::SGR_PIXELS_MOUSE),
            PointerMove {
                cell_changed: true,
                pixel_motion: true,
            }
        );
    }

    #[test]
    fn motion_is_reported_only_when_the_protocol_wants_it() {
        let drag = Mode::MOUSE_DRAG | Mode::SGR_PIXELS_MOUSE;
        let motion = Mode::MOUSE_MOTION | Mode::SGR_PIXELS_MOUSE;

        assert!(!motion_is_reported(drag, false));
        assert!(motion_is_reported(drag, true));
        assert!(motion_is_reported(motion, false));
        assert!(motion_is_reported(motion, true));
        assert!(!motion_is_reported(Mode::MOUSE_REPORT_CLICK, true));
        assert!(!motion_is_reported(Mode::empty(), true));
    }
}
