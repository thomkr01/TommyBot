//! Heat automod, join gate / raid detection and role-level lockdown.
//!
//! Everything except [`store`] is pure (time is passed in as unix ms), so it unit-tests
//! without a gateway or database. Ids are plain `u64` snowflakes to keep this crate
//! independent of the Discord client types.
pub mod discord_api;
pub mod filters;
pub mod heat;
pub mod join;
pub mod lockdown;
pub mod sites;
pub mod store;
pub mod strike;
pub mod window;
