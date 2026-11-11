//! Inline SVG icons from `assets/icons`, the same set the bot uses as custom emojis.
use leptos::prelude::*;

macro_rules! icons {
    ($($n:literal),*) => {
        fn svg(name: &str) -> &'static str {
            match name {
                $($n => include_str!(concat!("../../../../assets/icons/", $n, ".svg")),)*
                _ => "",
            }
        }
    };
}
icons!(
    "success",
    "error",
    "warning",
    "right_sort",
    "bell",
    "menu",
    "moon",
    "arrow_up",
    "arrow_down",
    "edit",
    "link",
    "page_prev",
    "page_next",
    "checkbox_off",
    "modmail",
    "info",
    "setup_wizard", "sparkle", "fingerprint", "logs", "tray", "channel_text", "lock", "page", "ban",
    "globe", "sliders", "discord", "tag", "shield", "hand", "member_join", "glasses", "rocket",
    "qr_code", "moderator", "case", "note", "report", "ticket", "target", "settings", "restore",
    "history", "palette", "rescue_key", "chevron_right", "chat_shield", "announcement", "age_18",
    "word_censor",
    "backup", "trash", "back", "channel_voice", "category", "channel_stage", "forum", "server", "verification", "nsfw_filter", "webhook", "role", "dropdown",
    "suspicious_account"
);

/// Every copy gets its own gradient and mask ids: with shared ids every copy paints with the
/// first one's, which draws nothing once that one is hidden (closed nav group, hidden tab).
pub fn icon(name: &'static str) -> AnyView {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let svg = svg(name).replace(&format!("\"{name}-"), &format!("\"{name}{n}-")).replace(&format!("#{name}-"), &format!("#{name}{n}-"));
    view! { <span class="i" aria-hidden="true" inner_html=svg></span> }.into_any()
}
