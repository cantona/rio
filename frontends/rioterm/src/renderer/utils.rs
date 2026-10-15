use crate::constants;
use crate::layout::ContextDimension;
use rio_backend::config::navigation::Navigation;
use rio_backend::config::Config;
use rio_window::window::Theme;

#[inline]
pub fn padding_top_from_config(
    navigation: &Navigation,
    padding_y_top: f32,
    #[allow(unused)] num_tabs: usize,
    #[allow(unused)] macos_use_unified_titlebar: bool,
) -> f32 {
    // When navigation is enabled (Tab mode), start content below island
    if navigation.is_enabled() {
        // On Linux/Windows, if hide_if_single is true and there's only one tab,
        // the island is hidden so render from 0 + configured margin
        #[cfg(not(target_os = "macos"))]
        if navigation.hide_if_single && num_tabs <= 1 {
            return constants::PADDING_Y + padding_y_top;
        }

        return navigation.tab_bar_height + padding_y_top;
    }

    let default_padding = constants::PADDING_Y + padding_y_top;

    #[cfg(target_os = "macos")]
    {
        use rio_backend::config::navigation::NavigationMode;
        if navigation.mode == NavigationMode::NativeTab {
            let additional = if macos_use_unified_titlebar {
                constants::ADDITIONAL_PADDING_Y_ON_UNIFIED_TITLEBAR
            } else {
                0.0
            };
            return additional + padding_y_top;
        }
    }

    default_padding
}

/// The pane's size for `TIOCSWINSZ` and `CSI 14 t`: cells, and the text area
/// in physical pixels as `columns * cell_width` by `lines * cell_height`, the
/// same area SGR-Pixels mouse reports are measured in.
#[inline]
pub fn terminal_dimensions(layout: &ContextDimension) -> rio_backend::event::WindowSize {
    let width = layout.columns * layout.cell.cell_width as usize;
    let height = layout.lines * layout.cell.cell_height as usize;
    rio_backend::event::WindowSize {
        width: u16::try_from(width).unwrap_or(u16::MAX),
        height: u16::try_from(height).unwrap_or(u16::MAX),
        cols: layout.columns as u16,
        rows: layout.lines as u16,
    }
}

#[inline]
pub fn update_colors_based_on_theme(config: &mut Config, theme_opt: Option<Theme>) {
    if let Some(theme) = theme_opt {
        if let Some(adaptive_colors) = &config.adaptive_colors {
            match theme {
                Theme::Light => {
                    if let Some(light_colors) = adaptive_colors.light {
                        config.colors = light_colors;
                    }
                }
                Theme::Dark => {
                    if let Some(darkcolors) = adaptive_colors.dark {
                        config.colors = darkcolors;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_backend::config::layout::Margin;

    #[test]
    fn terminal_dimensions_cover_whole_cells_only() {
        // A 1000x700 split with an 11x23 cell fits 90 columns and 30 lines;
        // the leftover 10 and 10 pixels are padding, not text area.
        let mut layout = ContextDimension {
            width: 1000.0,
            height: 700.0,
            columns: 90,
            lines: 30,
            margin: Margin::all(0.0),
            ..Default::default()
        };
        layout.cell.cell_width = 11;
        layout.cell.cell_height = 23;

        let size = terminal_dimensions(&layout);
        assert_eq!((size.cols, size.rows), (90, 30));
        assert_eq!((size.width, size.height), (990, 690));

        // The margin does not enter into it either.
        layout.margin = Margin::all(10.0);
        assert_eq!(terminal_dimensions(&layout), size);
    }

    #[test]
    fn terminal_dimensions_saturate_pixels() {
        let mut layout = ContextDimension {
            columns: 2000,
            lines: 1000,
            ..Default::default()
        };
        layout.cell.cell_width = 40;
        layout.cell.cell_height = 80;

        let size = terminal_dimensions(&layout);
        assert_eq!((size.width, size.height), (u16::MAX, u16::MAX));
    }
}
