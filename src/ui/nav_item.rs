//! Static definition of navigation items shown in the dock.

use std::str::FromStr;

/// Typed identifier for navigation items — replaces string-based label matching.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NavItemId {
    #[default]
    Chat,
    Totp,
    Ai,
    Calendar,
    Calls,
    Account,
}

impl NavItemId {
    /// Persisted string form of every item, in variant-declaration order.
    const NAMES: [&'static str; 6] = ["Chat", "TOTP", "AI", "Calendar", "Calls", "Account"];

    /// String representation for persistence and DHT sync.
    pub const fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

impl FromStr for NavItemId {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        NAV_ITEMS
            .iter()
            .find(|item| item.id.as_str() == s)
            .map(|item| item.id)
            .ok_or(())
    }
}

/// Icon, label, and route path for a single navigation entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NavItemData {
    pub id: NavItemId,
    pub icon: &'static str,
    pub label: &'static str,
    pub route: &'static str,
}

/// Canonical navigation items in their default display order.
pub static NAV_ITEMS: [NavItemData; 6] = [
    NavItemData {
        id: NavItemId::Chat,
        icon: "icon-[line-md--chat-twotone]",
        label: "Chat",
        route: "/chat",
    },
    NavItemData {
        id: NavItemId::Totp,
        icon: "icon-[line-md--security-twotone]",
        label: "TOTP",
        route: "/totp",
    },
    NavItemData {
        id: NavItemId::Ai,
        icon: "icon-[line-md--lightbulb-twotone]",
        label: "AI",
        route: "/ai",
    },
    NavItemData {
        id: NavItemId::Calendar,
        icon: "icon-[line-md--calendar]",
        label: "Calendar",
        route: "/calendar",
    },
    NavItemData {
        id: NavItemId::Calls,
        icon: "icon-[line-md--phone-call-twotone]",
        label: "Calls",
        route: "/calls",
    },
    NavItemData {
        id: NavItemId::Account,
        icon: "icon-[line-md--account]",
        label: "Account",
        route: "/account",
    },
];
