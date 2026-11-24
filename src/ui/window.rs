//! Wayland layer-shell overlay window hosting the floating wavepill capsule.

use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::config::{Config, Corner};

use super::island::IslandWidget;

const OVERLAY_NAMESPACE: &str = "wavepill";
const WINDOW_REVEAL_MS: u32 = 200;

/// Layer-shell edge anchors and margins derived from [`Corner`] and config offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerPlacement {
    pub vertical_edge: Edge,
    pub horizontal_edge: Edge,
    pub vertical_margin: i32,
    pub horizontal_margin: i32,
}

/// Computes the two perpendicular screen edges and margins for a given [`Corner`].
pub fn layer_placement(corner: Corner, margin_x: i32, margin_y: i32) -> LayerPlacement {
    let vertical_edge = match corner {
        Corner::TopLeft | Corner::TopRight => Edge::Top,
        Corner::BottomLeft | Corner::BottomRight => Edge::Bottom,
    };
    let horizontal_edge = match corner {
        Corner::TopLeft | Corner::BottomLeft => Edge::Left,
        Corner::TopRight | Corner::BottomRight => Edge::Right,
    };

    LayerPlacement {
        vertical_edge,
        horizontal_edge,
        vertical_margin: margin_y,
        horizontal_margin: margin_x,
    }
}

/// Transparent overlay `ApplicationWindow` anchored to a screen corner via `wlr-layer-shell`.
#[derive(Clone)]
pub struct WavepillWindow {
    window: gtk4::ApplicationWindow,
    outer_revealer: gtk4::Revealer,
    island: IslandWidget,
}

impl WavepillWindow {
    /// Creates and configures the layer-shell overlay window containing `island`.
    ///
    /// Starts hidden (`window.is_visible() == false`) so it never intercepts corner input
    /// until audio or media activity reveals it.
    pub fn new(app: &gtk4::Application, config: &Config, island: IslandWidget) -> Self {
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("wavepill")
            .decorated(false)
            .resizable(false)
            .build();
        window.add_css_class("wavepill-window");
        window.set_can_focus(false);

        if gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_layer(Layer::Overlay);
            window.set_keyboard_mode(KeyboardMode::None);
            window.set_namespace(Some(OVERLAY_NAMESPACE));

            let placement = layer_placement(config.corner, config.margin_x, config.margin_y);
            for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                window.set_anchor(edge, false);
                window.set_margin(edge, 0);
            }
            window.set_anchor(placement.vertical_edge, true);
            window.set_anchor(placement.horizontal_edge, true);
            window.set_margin(placement.vertical_edge, placement.vertical_margin);
            window.set_margin(placement.horizontal_edge, placement.horizontal_margin);
        } else {
            tracing::warn!(
                "gtk4-layer-shell is not supported by the current display server; running as undecorated window"
            );
        }

        let outer_revealer = gtk4::Revealer::new();
        outer_revealer.set_transition_type(gtk4::RevealerTransitionType::Crossfade);
        outer_revealer.set_transition_duration(WINDOW_REVEAL_MS);
        outer_revealer.set_reveal_child(false);
        outer_revealer.set_child(Some(island.widget()));

        window.set_child(Some(&outer_revealer));
        window.set_visible(false);

        {
            let win_weak = window.downgrade();
            let island_clone = island.clone();
            outer_revealer.connect_child_revealed_notify(move |revealer| {
                if !revealer.reveals_child()
                    && !revealer.is_child_revealed()
                    && let Some(win) = win_weak.upgrade()
                {
                    win.set_visible(false);
                    island_clone.set_active(false);
                }
            });
        }

        Self {
            window,
            outer_revealer,
            island,
        }
    }

    /// Shows or hides the floating overlay window with a smooth crossfade transition.
    pub fn set_overlay_visible(&self, visible: bool) {
        if visible {
            self.island.set_active(true);
            if !self.window.is_visible() {
                self.window.set_visible(true);
            }
            if !self.outer_revealer.reveals_child() {
                self.outer_revealer.set_reveal_child(true);
            }
        } else {
            if self.outer_revealer.reveals_child() {
                self.outer_revealer.set_reveal_child(false);
            }
            if !self.outer_revealer.is_child_revealed() {
                if self.window.is_visible() {
                    self.window.set_visible(false);
                }
                self.island.set_active(false);
            }
        }
    }

    /// Returns a reference to the underlying GTK4 `ApplicationWindow`.
    pub fn widget(&self) -> &gtk4::ApplicationWindow {
        &self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_four_corners_to_expected_edges_and_margins() {
        assert_eq!(
            layer_placement(Corner::TopRight, 16, 48),
            LayerPlacement {
                vertical_edge: Edge::Top,
                horizontal_edge: Edge::Right,
                vertical_margin: 48,
                horizontal_margin: 16,
            }
        );
        assert_eq!(
            layer_placement(Corner::TopLeft, 12, 24),
            LayerPlacement {
                vertical_edge: Edge::Top,
                horizontal_edge: Edge::Left,
                vertical_margin: 24,
                horizontal_margin: 12,
            }
        );
        assert_eq!(
            layer_placement(Corner::BottomRight, 20, 32),
            LayerPlacement {
                vertical_edge: Edge::Bottom,
                horizontal_edge: Edge::Right,
                vertical_margin: 32,
                horizontal_margin: 20,
            }
        );
        assert_eq!(
            layer_placement(Corner::BottomLeft, 8, 10),
            LayerPlacement {
                vertical_edge: Edge::Bottom,
                horizontal_edge: Edge::Left,
                vertical_margin: 10,
                horizontal_margin: 8,
            }
        );
    }
}
