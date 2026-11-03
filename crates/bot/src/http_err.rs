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

/// Discord's JSON error code (50035 invalid form, 30013 too many channels, ...).
pub fn code(e: &anyhow::Error) -> Option<u64> {
    e.downcast_ref::<Error>()
        .and_then(status_code)
        .and_then(|(_, c)| c)
}

/// 404, or 403 "Missing Access": the bot is not in that server (any more).
pub fn no_access(e: &Error) -> bool {
    status_code(e).is_some_and(|(s, _)| s == 403 || s == 404)
}

/// The message, member, channel, role, ban or webhook no longer exists.
pub fn gone(e: &Error) -> bool {
    status_code(e).is_some_and(|(s, _)| s == 404)
}

/// [`gone`] for an error that went through `?`.
pub fn gone_any(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Error>().is_some_and(gone)
}

/// Discord's reason for a failed action, for the reply.
pub fn why(e: &anyhow::Error) -> String {
    match e.downcast_ref::<Error>().map(|e| e.kind()) {
        Some(ErrorType::Response {
            error: ApiError::General(g),
            ..
        }) => match g.code {
            50013 => {
                "missing permissions (their top role is above mine, or they have Administrator)"
                    .into()
            }
            _ => g.message.clone(),
        },
        _ => "see logs".into(),
    }
}

fn expected(e: &Error) -> bool {
    gone(e) || status_code(e).is_some_and(|(_, c)| c == Some(DMS_CLOSED))
}

/// For calls whose failure must not stop the caller. Unexpected failures are logged so a
/// missing permission never fails silently.
pub fn best_effort<T>(r: Result<T, Error>, what: &str) -> Option<T> {
    r.inspect_err(|e| {
        if !expected(e) {
            tracing::warn!(?e, what, "discord call failed");
        }
    })
    .ok()
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
