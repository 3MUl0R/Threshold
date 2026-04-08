//! Tray context menu construction.

use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};

use crate::icons::TrayState;

/// IDs for menu items, used to match events.
#[allow(dead_code)] // launch_at_login used in Phase 17C
pub struct MenuItems {
    pub menu: Menu,
    pub open_dashboard: MenuItem,
    pub start: MenuItem,
    pub restart: MenuItem,
    pub stop: MenuItem,
    pub launch_at_login: CheckMenuItem,
    pub quit: MenuItem,
}

impl MenuItems {
    /// Build the tray context menu.
    pub fn new() -> Self {
        let menu = Menu::new();

        let open_dashboard = MenuItem::new("Open Dashboard", true, None);
        let start = MenuItem::new("Start", false, None);
        let restart = MenuItem::new("Restart", false, None);
        let stop = MenuItem::new("Stop", false, None);
        let launch_at_login = CheckMenuItem::new("Launch at Login", true, false, None);
        let quit = MenuItem::new("Quit Tray", true, None);

        menu.append_items(&[
            &open_dashboard,
            &PredefinedMenuItem::separator(),
            &start,
            &restart,
            &stop,
            &PredefinedMenuItem::separator(),
            // launch_at_login is hidden until Phase 17C wires up the toggle
            &quit,
        ])
        .expect("Failed to build tray menu");

        MenuItems {
            menu,
            open_dashboard,
            start,
            restart,
            stop,
            launch_at_login,
            quit,
        }
    }

    /// Update menu item enabled/disabled states based on daemon state.
    pub fn update_for_state(&self, state: TrayState) {
        match state {
            TrayState::Running => {
                self.open_dashboard.set_enabled(true);
                self.start.set_enabled(false);
                self.restart.set_enabled(true);
                self.stop.set_enabled(true);
            }
            TrayState::Draining => {
                self.open_dashboard.set_enabled(false);
                self.start.set_enabled(false);
                self.restart.set_enabled(false);
                self.stop.set_enabled(false);
            }
            TrayState::Stopped | TrayState::Error => {
                self.open_dashboard.set_enabled(false);
                self.start.set_enabled(true);
                self.restart.set_enabled(false);
                self.stop.set_enabled(false);
            }
        }
    }
}
