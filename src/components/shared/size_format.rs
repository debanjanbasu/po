//! Size and count formatting helpers (binary byte sizes, compact counts).

/// Formats a byte size using binary units (1 KB = 1024 B), truncating to whole
/// units (e.g. "512 B", "3 KB", "5 MB", "2 GB").
///
/// Used for snapshot/export sizes, where whole-unit precision is enough and a
/// "k"/"M" compact form would be misleading about actual storage.
#[must_use]
pub fn format_binary_bytes(size: u32) -> String {
    const KB: u32 = 1024;
    const MB: u32 = 1024 * KB;
    const GB: u32 = 1024 * MB;
    if size < KB {
        format!("{size} B")
    } else if size < MB {
        format!("{} KB", size / KB)
    } else if size < GB {
        format!("{} MB", size / MB)
    } else {
        format!("{} GB", size / GB)
    }
}

/// Formats a large counter compactly with an order-of-magnitude marker and a
/// tilde, keeping one decimal (e.g. `999` → `<1k`, `12_345` → `~12.3k`).
///
/// Used for rough peer/network estimates where the value is a live guess, so
/// the "~" prefix signals approximation rather than an exact count.
#[must_use]
pub fn format_compact_count(size: u32) -> String {
    const THOUSAND: u32 = 1_000;
    if size < THOUSAND {
        "<1k".to_string()
    } else {
        let tenths = size / 100;
        format!("~{}.{}k", tenths / 10, tenths % 10)
    }
}
