//! Route enum definitions for auth, onboarding, and main app flows.

use crate::{
    components::{
        Account, Ai, Calendar, Calls, Chat, ChatConversation, LoginSignup, PageNotFound, Pairing,
        Profile, Security, Snapshots, Totp,
    },
    router::nav_layout::NavLayout,
};
use dioxus::prelude::*;

/// Routes available before the user is authenticated.
/// Any stale URL (e.g. `/totp` from a previous session) also shows login.
#[derive(Routable, Clone, Default, PartialEq, Eq)]
#[rustfmt::skip]
pub enum AuthRoute {
    #[redirect("/:.._route", |_route: Vec<String>| AuthRoute::LoginSignup)]
    #[route("/")]
    #[default]
    LoginSignup,
    #[route("/pair")]
    Pairing,
}

/// Main application routes, wrapped in `NavLayout` for the dock navigation.
#[derive(Routable, Clone, Default, PartialEq, Eq)]
#[rustfmt::skip]
pub enum Route {
    #[layout(NavLayout)]
        #[redirect("/", || Route::Chat)]
        #[redirect("/pair", || Route::Pairing)]
        #[redirect("/login", || Route::Chat)]
        #[route("/totp")]
        Totp,
        #[nest("/account")]
            #[route("/")]
            Account,
            #[route("/profile")]
            Profile,
            #[route("/security")]
            Security,
            #[route("/snapshots")]
            Snapshots,
            #[route("/pair")]
            Pairing,
        #[end_nest]
        #[route("/chat")]
        #[default]
        Chat,
        #[route("/chat/:conversation_key")]
        ChatConversation { conversation_key: String },
        #[route("/ai")]
        Ai,
        #[route("/calendar")]
        Calendar,
        #[route("/calls")]
        Calls,
        #[route("/:..route")]
        PageNotFound { route: Vec<String> },
}
