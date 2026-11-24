//! Wayland layer-shell overlay window hosting the floating wavepill capsule.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::{gdk, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::config::{Config, Corner};

use super::island::IslandWidget;

const OVERLAY_NAMESPACE: &str = "wavepill";
const WINDOW_REVEAL_MS: u32 = 200;
const DRAG_THRESHOLD_PX: f64 = 6.0;
const FALLBACK_MAX_BOUND_X: i32 = 3840;
const FALLBACK_MAX_BOUND_Y: i32 = 2160;

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

/// Applies a pointer drag delta `(dx, dy)` in screen coordinates to `(margin_x, margin_y)`
/// anchored at `corner`, clamping within `[0, max_bounds]`.
pub fn apply_drag_delta(
    corner: Corner,
    current: (i32, i32),
    delta: (i32, i32),
    max_bounds: (i32, i32),
) -> (i32, i32) {
    let horizontal_sign = match corner {
        Corner::TopLeft | Corner::BottomLeft => 1,
        Corner::TopRight | Corner::BottomRight => -1,
    };
    let vertical_sign = match corner {
        Corner::TopLeft | Corner::TopRight => 1,
        Corner::BottomLeft | Corner::BottomRight => -1,
    };

    let next_x = current
        .0
        .saturating_add(delta.0.saturating_mul(horizontal_sign))
        .clamp(0, max_bounds.0.max(0));
    let next_y = current
        .1
        .saturating_add(delta.1.saturating_mul(vertical_sign))
        .clamp(0, max_bounds.1.max(0));

    (next_x, next_y)
}

/// Transparent overlay `ApplicationWindow` anchored to a screen corner via `wlr-layer-shell`.
#[derive(Clone)]
pub struct WavepillWindow {
    window: gtk4::ApplicationWindow,
    outer_revealer: gtk4::Revealer,
    island: IslandWidget,
    corner: Corner,
    margins: Rc<Cell<(i32, i32)>>,
    config: Rc<RefCell<Config>>,
}

impl WavepillWindow {
    /// Creates and configures the layer-shell overlay window containing `island`,
    /// including mouse drag-and-drop repositioning via layer-shell margins.
    ///
    /// Starts hidden (`window.is_visible() == false`) so it never intercepts corner input
    /// until audio or media activity reveals it.
    pub fn new(
        app: &gtk4::Application,
        config: &Config,
        island: IslandWidget,
        on_dragging: Rc<dyn Fn(bool)>,
    ) -> Self {
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("wavepill")
            .decorated(false)
            .resizable(false)
            .build();
        window.add_css_class("wavepill-window");
        window.set_can_focus(false);

        let corner = config.corner;
        let initial_margins = (config.margin_x.max(0), config.margin_y.max(0));
        let margins = Rc::new(Cell::new(initial_margins));
        let config_state = Rc::new(RefCell::new(config.clone()));

        if gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_layer(Layer::Overlay);
            window.set_keyboard_mode(KeyboardMode::None);
            window.set_namespace(Some(OVERLAY_NAMESPACE));

            let placement = layer_placement(corner, initial_margins.0, initial_margins.1);
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

        attach_drag_controller(
            &window,
            &island,
            corner,
            Rc::clone(&margins),
            Rc::clone(&config_state),
            on_dragging,
        );

        Self {
            window,
            outer_revealer,
            island,
            corner,
            margins,
            config: config_state,
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

    /// Resets the window position back to the default corner margins and persists it.
    pub fn reset_position(&self) {
        let defaults = Config::default();
        let next = (defaults.margin_x, defaults.margin_y);
        self.margins.set(next);
        apply_window_margins(&self.window, self.corner, next);

        let mut cfg = self.config.borrow_mut();
        cfg.margin_x = next.0;
        cfg.margin_y = next.1;
        if let Err(err) = cfg.save_default() {
            tracing::warn!(error = %err, "Failed to persist reset wavepill position");
        }
    }

    /// Returns a reference to the underlying GTK4 `ApplicationWindow`.
    pub fn widget(&self) -> &gtk4::ApplicationWindow {
        &self.window
    }
}

fn attach_drag_controller(
    window: &gtk4::ApplicationWindow,
    island: &IslandWidget,
    corner: Corner,
    margins: Rc<Cell<(i32, i32)>>,
    config: Rc<RefCell<Config>>,
    on_dragging: Rc<dyn Fn(bool)>,
) {
    let drag = gtk4::GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);
    drag.set_propagation_phase(gtk4::PropagationPhase::Bubble);

    let drag_active = Rc::new(Cell::new(false));
    // Stores the latest surface-local drag offset to apply at most once per compositor frame,
    // avoiding multi-counting motion events queued before Wayland commits the new margins.
    let pending_offset: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
    let tick_scheduled = Rc::new(Cell::new(false));

    {
        let drag_active = Rc::clone(&drag_active);
        let pending_offset = Rc::clone(&pending_offset);
        drag.connect_drag_begin(move |_, _, _| {
            drag_active.set(false);
            pending_offset.set(None);
        });
    }

    {
        let win_weak = window.downgrade();
        let capsule_weak = island.widget().downgrade();
        let drag_active = Rc::clone(&drag_active);
        let pending_offset = Rc::clone(&pending_offset);
        let tick_scheduled = Rc::clone(&tick_scheduled);
        let margins = Rc::clone(&margins);
        let on_dragging = Rc::clone(&on_dragging);

        drag.connect_drag_update(move |_, offset_x, offset_y| {
            if !drag_active.get() {
                if offset_x.hypot(offset_y) < DRAG_THRESHOLD_PX {
                    return;
                }
                drag_active.set(true);
                if let Some(capsule) = capsule_weak.upgrade() {
                    capsule.add_css_class("dragging");
                }
                on_dragging(true);
            }

            pending_offset.set(Some((offset_x, offset_y)));

            if tick_scheduled.replace(true) {
                return;
            }

            let Some(win) = win_weak.upgrade() else {
                tick_scheduled.set(false);
                return;
            };

            let pending_for_tick = Rc::clone(&pending_offset);
            let scheduled_for_tick = Rc::clone(&tick_scheduled);
            let margins_for_tick = Rc::clone(&margins);
            let active_for_tick = Rc::clone(&drag_active);
            win.add_tick_callback(move |win, _| {
                scheduled_for_tick.set(false);
                if !active_for_tick.get() {
                    pending_for_tick.set(None);
                    return gtk4::glib::ControlFlow::Break;
                }
                if let Some((ox, oy)) = pending_for_tick.take() {
                    let dx = ox.round() as i32;
                    let dy = oy.round() as i32;
                    if dx != 0 || dy != 0 {
                        let max_bounds = monitor_drag_bounds(win);
                        let next =
                            apply_drag_delta(corner, margins_for_tick.get(), (dx, dy), max_bounds);
                        if next != margins_for_tick.get() {
                            margins_for_tick.set(next);
                            apply_window_margins(win, corner, next);
                        }
                    }
                }
                gtk4::glib::ControlFlow::Break
            });
        });
    }

    {
        let capsule_weak = island.widget().downgrade();
        let drag_active = Rc::clone(&drag_active);
        let pending_offset = Rc::clone(&pending_offset);
        let margins = Rc::clone(&margins);
        let config = Rc::clone(&config);
        let on_dragging = Rc::clone(&on_dragging);

        drag.connect_drag_end(move |_, _, _| {
            pending_offset.set(None);
            if !drag_active.replace(false) {
                return;
            }
            if let Some(capsule) = capsule_weak.upgrade() {
                capsule.remove_css_class("dragging");
            }
            on_dragging(false);

            let (mx, my) = margins.get();
            let mut cfg = config.borrow_mut();
            cfg.margin_x = mx;
            cfg.margin_y = my;
            if let Err(err) = cfg.save_default() {
                tracing::warn!(error = %err, "Failed to persist dragged wavepill position");
            }
        });
    }

    {
        let capsule_weak = island.widget().downgrade();
        let drag_active = Rc::clone(&drag_active);
        let pending_offset = Rc::clone(&pending_offset);
        let on_dragging = Rc::clone(&on_dragging);

        drag.connect_cancel(move |_, _| {
            pending_offset.set(None);
            if !drag_active.replace(false) {
                return;
            }
            if let Some(capsule) = capsule_weak.upgrade() {
                capsule.remove_css_class("dragging");
            }
            on_dragging(false);
        });
    }

    island.widget().add_controller(drag);
}

fn apply_window_margins(window: &gtk4::ApplicationWindow, corner: Corner, margins: (i32, i32)) {
    if !gtk4_layer_shell::is_supported() {
        return;
    }
    let placement = layer_placement(corner, margins.0, margins.1);
    window.set_margin(placement.horizontal_edge, placement.horizontal_margin);
    window.set_margin(placement.vertical_edge, placement.vertical_margin);
}

fn monitor_drag_bounds(window: &gtk4::ApplicationWindow) -> (i32, i32) {
    let win_w = window.width().max(1);
    let win_h = window.height().max(1);

    if let Some(surface) = window.surface()
        && let Some(display) = gdk::Display::default()
        && let Some(monitor) = display.monitor_at_surface(&surface)
    {
        let geom = monitor.geometry();
        return (
            (geom.width() - win_w).max(0),
            (geom.height() - win_h).max(0),
        );
    }

    (FALLBACK_MAX_BOUND_X, FALLBACK_MAX_BOUND_Y)
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

    #[test]
    fn apply_drag_delta_respects_corner_signs_and_clamps_bounds() {
        // TopRight: dragging left (-dx) increases right margin; dragging down (+dy) increases top margin.
        assert_eq!(
            apply_drag_delta(Corner::TopRight, (16, 48), (-100, 50), (1600, 900)),
            (116, 98)
        );
        // Dragging past right/top edges clamps at 0.
        assert_eq!(
            apply_drag_delta(Corner::TopRight, (16, 48), (100, -100), (1600, 900)),
            (0, 0)
        );

        // TopLeft: +dx increases left margin, +dy increases top margin.
        assert_eq!(
            apply_drag_delta(Corner::TopLeft, (10, 20), (30, 40), (1600, 900)),
            (40, 60)
        );

        // BottomRight: -dx increases right margin, -dy increases bottom margin.
        assert_eq!(
            apply_drag_delta(Corner::BottomRight, (10, 20), (-25, -35), (1600, 900)),
            (35, 55)
        );

        // BottomLeft: +dx increases left margin, -dy increases bottom margin; clamps upper bound.
        assert_eq!(
            apply_drag_delta(Corner::BottomLeft, (10, 20), (5000, -5000), (800, 600)),
            (800, 600)
        );
    }
}
