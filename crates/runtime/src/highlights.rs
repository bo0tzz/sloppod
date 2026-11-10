//! Snoozed highlights coming back (#467): the user puts one off until later, and the runtime brings it back when its
//! time passes, so it counts as waiting on them again.

use std::time::Duration;

use crate::{AppState, db};

/// How often a snooze is checked for having run out: about when it's due, rather than at the minute.
const EVERY: Duration = Duration::from_secs(60);

pub async fn run(state: AppState) {
    let mut every = tokio::time::interval(EVERY);
    loop {
        every.tick().await;
        wake_due(&state).await;
    }
}

/// Brings back the highlights whose snooze has run out, telling the session each is in and the owner's connections:
/// it's in the user's "for you" list again and counts as waiting on them. No turn starts and the agent isn't told, as
/// when it was snoozed.
async fn wake_due(state: &AppState) {
    let due = match db::due_highlights(&state.db).await {
        Ok(due) => due,
        Err(e) => {
            tracing::warn!(error = %e, "listing highlights whose snooze has run out");
            return;
        }
    };
    for row in due {
        // Tells the session's UIs the highlight as it is now, and its owner's connections that what waits changed.
        state.hub.broadcast_highlight(&row.to_api());
    }
}
