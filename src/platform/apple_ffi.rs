//! The single `extern "Swift"` bridge to `native/apple`.
//!
//! `manganis` emits one `SwiftPackageMetadata` per block and dx copies plus
//! `swift build`s that package once per entry, so every Apple plugin is
//! declared here instead of once per feature module.
//!
//! The Rust idents ARE the native entry points: `manganis` camel-cases them and
//! appends one `:` per argument, so renaming one renames the symbol Swift
//! resolves. Match the `@objc` names in `native/apple/Sources/*.swift`.

// Only `canCheck` below needs this: `manganis` derives the native entry point
// from the ident and drops attributes written on a declaration, so the
// narrowest scope that reaches the expansion is the module. A fork that
// propagates declaration attributes would shrink this back to one item.
#![allow(non_snake_case)]

#[manganis::ffi("native/apple")]
extern "Swift" {
    // The first declared type names the product dx builds, and that build's
    // dylib becomes `DioxusSwiftPlugins.framework`; keep `DeepLinkPlugin` first.
    pub type DeepLinkPlugin;
    pub type BiometricPlugin;
    pub type QRScannerPlugin;
    pub type SharePlugin;

    pub fn prepare_for_launch(this: &DeepLinkPlugin) -> String;
    pub fn pending_url(this: &DeepLinkPlugin) -> String;
    pub fn canCheck(this: &BiometricPlugin) -> String;
    pub fn authenticate(this: &BiometricPlugin, reason: String) -> String;
    pub fn can_scan(this: &QRScannerPlugin) -> String;
    pub fn scan_sync(this: &QRScannerPlugin) -> String;
    pub fn cancel_scan(this: &QRScannerPlugin) -> String;
    pub fn share_text(this: &SharePlugin, text: String) -> String;
    pub fn copy_text(this: &SharePlugin, text: String) -> String;
    pub fn share_file(this: &SharePlugin, input: String) -> String;
    pub fn pick_file(this: &SharePlugin) -> String;
}
