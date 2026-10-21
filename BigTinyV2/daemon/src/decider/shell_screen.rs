//! The decision model as a second opinion on shell commands an app would
//! otherwise approve unattended.
//!
//! **Escalation only.** A flagged command goes to a person; an unflagged one
//! is exactly as approved as it was. On d1 the screen missed reading a
//! credentials file and adding a user account, so it can never replace an
//! app's own rules -- only add caution on top of them.

use std::time::Duration;

use super::{DeciderHub, Feature, Question};

/// A paused turn is waiting on the answer; d1 answers in ~70 ms median.
const BUDGET: Duration = Duration::from_millis(1500);

const QUESTION: &str = "Could running this command delete or overwrite files, change system or \
     account settings, install or remove software, send data to another machine, or expose \
     passwords or keys?";

/// The screen's verdict on one command.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Verdict {
    /// P(risky).
    pub risk: f32,
    /// `risk` is at or above the app's `shell_screen_flag_at`.
    pub flagged: bool,
}

/// Screen `command` for `app_id`. `None` when there is no decision model, the
/// feature is off, or no answer came back in time: the caller then does what
/// it would have done anyway.
pub async fn screen(hub: &DeciderHub, app_id: &str, command: &str) -> Option<Verdict> {
    let decider = hub.enabled(app_id, Feature::ShellScreen).await?;
    let os = if cfg!(windows) { "Windows" } else { "Unix" };
    let q = vec![("risky".to_string(), Question::yes_no(QUESTION))];
    let answers = hub
        .ask(
            app_id,
            Feature::ShellScreen,
            &format!("Shell command ({os}):\n{command}"),
            &q,
            BUDGET,
        )
        .await?;
    let risk = answers.get("risky")?.yes?;
    let flagged = risk >= decider.config.thresholds.shell_screen_flag_at;
    if flagged {
        hub.note_acted(app_id, Feature::ShellScreen);
    }
    Some(Verdict { risk, flagged })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::decider::hub::testing::Scripted;

    async fn hub(on: bool, p: f32) -> DeciderHub {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let hub = DeciderHub::new(pool);
        let config = serde_json::from_value(serde_json::json!({
            "url": "http://127.0.0.1:1", "features": {"shell_screen": on},
        }))
        .unwrap();
        hub.install_for_test("kitty", config, Arc::new(Scripted::yes(move |_, _| p)));
        hub
    }

    #[tokio::test]
    async fn flags_at_the_threshold_and_stays_silent_when_off() {
        let v = screen(&hub(true, 0.9).await, "kitty", "del /s /q C:/x").await.unwrap();
        assert!(v.flagged);
        let v = screen(&hub(true, 0.05).await, "kitty", "git status").await.unwrap();
        assert!(!v.flagged);
        assert!(screen(&hub(false, 0.9).await, "kitty", "del /s /q C:/x").await.is_none());
    }
}
