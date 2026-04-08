//! Programmatic icon generation for the system tray.
//!
//! Generates simple colored circle icons as RGBA pixel data — no external
//! image files needed. Inspired by EchoType's `generate_circle_rgba` pattern.

use tray_icon::Icon;

/// Tray icon representing daemon state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// Daemon is running normally.
    Running,
    /// Daemon is draining (shutting down gracefully, e.g. during restart).
    Draining,
    /// Daemon is not running.
    Stopped,
    /// Cannot connect to daemon / unknown state.
    Error,
}

impl TrayState {
    /// Human-readable tooltip text for this state.
    pub fn tooltip(&self) -> &'static str {
        match self {
            TrayState::Running => "Threshold \u{2014} Running",
            TrayState::Draining => "Threshold \u{2014} Draining",
            TrayState::Stopped => "Threshold \u{2014} Stopped",
            TrayState::Error => "Threshold \u{2014} Unreachable",
        }
    }

    /// Color for this state as (R, G, B).
    fn color(&self) -> (u8, u8, u8) {
        match self {
            TrayState::Running => (76, 175, 80),    // Material green
            TrayState::Draining => (255, 193, 7),    // Material amber
            TrayState::Stopped => (158, 158, 158),   // Material grey
            TrayState::Error => (244, 67, 54),       // Material red
        }
    }
}

/// Generate a tray icon for the given state.
///
/// Creates a 32x32 filled circle with a 1px darker border.
pub fn icon_for_state(state: TrayState) -> Icon {
    let size: u32 = 32;
    let (r, g, b) = state.color();
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    let center = size as f64 / 2.0;
    let radius = center - 2.0; // 2px padding
    let border_radius = radius - 1.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f64 - center;
            let dy = y as f64 - center;
            let dist = (dx * dx + dy * dy).sqrt();

            let idx = ((y * size + x) * 4) as usize;

            if dist <= border_radius {
                // Fill color
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255;
            } else if dist <= radius {
                // Border: slightly darker
                rgba[idx] = r.saturating_sub(40);
                rgba[idx + 1] = g.saturating_sub(40);
                rgba[idx + 2] = b.saturating_sub(40);
                rgba[idx + 3] = 255;
            } else if dist <= radius + 1.0 {
                // Anti-alias edge
                let alpha = ((radius + 1.0 - dist) * 255.0) as u8;
                rgba[idx] = r.saturating_sub(40);
                rgba[idx + 1] = g.saturating_sub(40);
                rgba[idx + 2] = b.saturating_sub(40);
                rgba[idx + 3] = alpha;
            }
            // else: transparent (already zero)
        }
    }

    Icon::from_rgba(rgba, size, size).expect("Failed to create tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_states_produce_valid_icons() {
        for state in [
            TrayState::Running,
            TrayState::Draining,
            TrayState::Stopped,
            TrayState::Error,
        ] {
            let icon = icon_for_state(state);
            // If we get here without panic, the icon is valid
            drop(icon);
        }
    }

    #[test]
    fn tooltips_are_non_empty() {
        for state in [
            TrayState::Running,
            TrayState::Draining,
            TrayState::Stopped,
            TrayState::Error,
        ] {
            assert!(!state.tooltip().is_empty());
        }
    }
}
