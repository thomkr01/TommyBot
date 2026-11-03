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
icons!("success", "error", "warning", "right_sort", "bell", "menu", "moon", "arrow_up", "arrow_down", "edit", "link", "page_prev", "page_next", "checkbox_off", "modmail");

pub fn icon(name: &'static str) -> AnyView {
    view! { <span class="i" aria-hidden="true" inner_html=svg(name)></span> }.into_any()
}
