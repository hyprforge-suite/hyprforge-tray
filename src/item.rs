//! What a tray icon *is*, with no D-Bus in sight.
//!
//! Kept separate from [`crate::sni`] so the interesting question — which
//! icon does this state deserve, and what should the tooltip say — is a
//! pure function that can be asserted on without a bus, a bar, or a
//! radio. That is the same split `hyprforge-network` makes between its
//! model and its NetworkManager client, and for the same reason.

/// `org.kde.StatusNotifierItem.Category`.
///
/// Both of this suite's items are [`Category::Hardware`]: they report the
/// state of a radio, not of an application. Hosts use this to group and
/// order icons, so getting it wrong puts Wi-Fi among the chat apps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    ApplicationStatus,
    Communications,
    SystemServices,
    Hardware,
}

impl Category {
    /// The exact strings the spec defines. A host that does not
    /// recognise the value falls back to `ApplicationStatus` silently, so
    /// a typo here is invisible rather than an error.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::ApplicationStatus => "ApplicationStatus",
            Category::Communications => "Communications",
            Category::SystemServices => "SystemServices",
            Category::Hardware => "Hardware",
        }
    }
}

/// `org.kde.StatusNotifierItem.Status`.
///
/// [`Status::Passive`] asks the host to hide the icon. That is the right
/// answer for a radio that is switched off — a Bluetooth icon on a
/// machine with Bluetooth disabled is a permanent piece of noise — but it
/// is *not* the right answer for a radio that is on and unconnected,
/// which is exactly when someone goes looking for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passive,
    Active,
    NeedsAttention,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Passive => "Passive",
            Status::Active => "Active",
            Status::NeedsAttention => "NeedsAttention",
        }
    }
}

/// One tray icon's entire visible state.
///
/// Icons are named, never drawn. The name is resolved by the host against
/// the user's own icon theme, which `hyprforge-appearance` already sets
/// through gsettings — so the tray matches the desktop without this crate
/// owning a single pixel, and without a second place to configure what
/// the desktop already decides.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayItem {
    /// Stable across the life of the process; hosts key their own state
    /// on it.
    pub id: String,
    pub category: Category,
    pub status: Status,
    pub title: String,
    pub icon_name: String,
    /// First line of the tooltip — typically what is connected.
    pub tooltip_title: String,
    /// Second line, for the detail that does not fit in the first.
    pub tooltip_body: String,
}

impl TrayItem {
    /// What a left-click should run.
    ///
    /// Every item in this suite opens the Settings page it is about,
    /// which is why `--screen` exists. Returning the argument rather than
    /// spawning keeps this testable: asserting that the Bluetooth icon
    /// opens the Bluetooth page needs no process.
    pub fn activate_screen(&self) -> Option<&'static str> {
        match self.id.as_str() {
            "hyprforge-network" => Some("network"),
            "hyprforge-bluetooth" => Some("bluetooth"),
            // Night light has a Settings page of its own. It used to be a
            // tab of the Desktop screen, and naming the screen rather than
            // the tab once landed a click on Wallpaper instead.
            "hyprforge-night-light" => Some("night-light"),
            // Keep awake has its own Power screen now (see
            // `hyprforge-trayd`'s module doc) — this used to point at
            // `"idle"`, the Desktop screen's tab it lived on before the
            // move, and would have silently kept sending clicks there
            // forever if nothing had updated it alongside the move.
            "hyprforge-keep-awake" => Some("power"),
            // The battery and profile icon opens the same Power screen,
            // which shows both.
            "hyprforge-power" => Some("power"),
            "hyprforge-displays" => Some("displays"),
            // An id nothing claims falls through to `None`.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec's strings, spelled exactly. A host that does not
    /// recognise a category does not complain — it silently treats the
    /// item as an application, and the icon turns up in the wrong group
    /// with nothing logged anywhere.
    #[test]
    fn the_category_and_status_strings_are_the_ones_the_spec_defines() {
        assert_eq!(Category::Hardware.as_str(), "Hardware");
        assert_eq!(Category::SystemServices.as_str(), "SystemServices");
        assert_eq!(Status::Passive.as_str(), "Passive");
        assert_eq!(Status::NeedsAttention.as_str(), "NeedsAttention");
    }

    #[test]
    fn each_item_opens_the_settings_page_it_is_about() {
        let item = |id: &str| TrayItem {
            id: id.to_string(),
            category: Category::Hardware,
            status: Status::Active,
            title: String::new(),
            icon_name: String::new(),
            tooltip_title: String::new(),
            tooltip_body: String::new(),
        };
        assert_eq!(item("hyprforge-network").activate_screen(), Some("network"));
        assert_eq!(item("hyprforge-bluetooth").activate_screen(), Some("bluetooth"));
        assert_eq!(item("something-else").activate_screen(), None);
    }

    /// Each icon opens the page its own setting is on.
    ///
    /// Night light names its own page — when it was a tab of the Desktop
    /// screen, naming the screen instead once landed it on Wallpaper.
    /// Keep awake names the Power screen it now lives on, having moved
    /// off that same Desktop screen's `Idle` tab.
    #[test]
    fn every_icon_opens_the_page_its_own_setting_is_on() {
        let item = |id: &str| TrayItem {
            id: id.to_string(),
            category: Category::SystemServices,
            status: Status::Active,
            title: String::new(),
            icon_name: String::new(),
            tooltip_title: String::new(),
            tooltip_body: String::new(),
        };
        assert_eq!(
            item("hyprforge-night-light").activate_screen(),
            Some("night-light")
        );
        assert_eq!(item("hyprforge-keep-awake").activate_screen(), Some("power"));
        assert_eq!(item("hyprforge-power").activate_screen(), Some("power"));
        assert_eq!(item("hyprforge-displays").activate_screen(), Some("displays"));
        // An id nothing claims still resolves to nothing, rather than to
        // whichever arm happens to be last.
        assert_eq!(item("hyprforge-nonsense").activate_screen(), None);
    }
}
