//! Sorts Discord REST failures: a 404 means the target is already gone and closed DMs are normal;
//! anything else (missing permission, role too low, Discord down) is a real failure worth a log line.
use anyhow::Result;
use twilight_http::{api_error::ApiError, error::ErrorType, Client, Error};
use twilight_model::{
    guild::Member,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

/// Discord's "Cannot send messages to this user": the member closed their DMs.
const DMS_CLOSED: u64 = 50007;

fn status_code(e: &Error) -> Option<(u16, Option<u64>)> {
    match e.kind() {
        ErrorType::Response { status, error, .. } => Some((
            status.get(),
            match error {
                ApiError::General(g) => Some(g.code),
                _ => None,
            },
        )),
        _ => None,
    }
}

/// The message, member, channel, role, ban or webhook no longer exists.
pub fn gone(e: &Error) -> bool {
    status_code(e).is_some_and(|(s, _)| s == 404)
}

/// [`gone`] for an error that went through `?`.
pub fn gone_any(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Error>().is_some_and(gone)
}

fn expected(e: &Error) -> bool {
    gone(e) || status_code(e).is_some_and(|(_, c)| c == Some(DMS_CLOSED))
}

/// For calls whose failure must not stop the caller. Returns whether the call succeeded;
/// unexpected failures are logged so a missing permission never fails silently.
pub fn best_effort<T>(r: Result<T, Error>, what: &str) -> bool {
    match r {
        Ok(_) => true,
        Err(e) if expected(&e) => false,
        Err(e) => {
            tracing::warn!(?e, what, "discord call failed");
            false
        }
    }
}

/// A guild member, `None` only when Discord says they are not in the server. Any other failure
/// is an error, so a hiccup never makes staff look like members without permits.
pub async fn member(
    http: &Client,
    gid: Id<GuildMarker>,
    uid: Id<UserMarker>,
) -> Result<Option<Member>> {
    match http.guild_member(gid, uid).await {
        Ok(r) => Ok(Some(r.model().await?)),
        Err(e) if gone(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
