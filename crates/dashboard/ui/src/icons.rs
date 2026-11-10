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
    "info"
);

pub fn icon(name: &'static str) -> AnyView {
    view! { <span class="i" aria-hidden="true" inner_html=svg(name)></span> }.into_any()
}

/// Like `icon`, but its gradient and mask ids are unique, for icons shown many times on one page:
/// with shared ids every copy paints with the first one's, which draws nothing once that one is hidden.
pub fn icon_unique(name: &'static str) -> AnyView {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let svg = svg(name).replace(&format!("\"{name}-"), &format!("\"{name}{n}-")).replace(&format!("#{name}-"), &format!("#{name}{n}-"));
    view! { <span class="i" aria-hidden="true" inner_html=svg></span> }.into_any()
}
