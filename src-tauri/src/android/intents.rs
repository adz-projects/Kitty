//! What Android hands the app from outside: a share (text, images, files from
//! another app — A3) or a tapped notification (open that chat — A4).
//!
//! `KittyPlugin` captures each intent as it arrives (a cold start's launch
//! intent included) and copies shared files out of the sending app straight
//! away, while the read grant that came with the intent still holds. It
//! queues the results, and the webview collects them through
//! `commands::take_incoming` — asked for rather than pushed, because the
//! webview may not be listening yet when a share is what launched the app.
//!
//! # Never call this from the main thread
//!
//! Same rule as `secrets`: `run_mobile_plugin` blocks on the main looper.

use serde::{Deserialize, Serialize};

use super::handle;

/// One intent, as the webview gets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Incoming {
    /// Something shared into Kitty: text and copies of any files.
    Share {
        #[serde(default)]
        text: String,
        #[serde(default)]
        subject: String,
        #[serde(default)]
        paths: Vec<String>,
        /// Files that could not be read (the sending app refused).
        #[serde(default)]
        failed: u32,
    },
    /// A notification about this chat was tapped.
    OpenChat {
        #[serde(rename = "sessionId", alias = "session_id")]
        session_id: String,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IncomingBatch {
    #[serde(default)]
    pub intents: Vec<Incoming>,
    /// A share's files are still being copied: ask again shortly.
    #[serde(default)]
    pub copying: bool,
}

/// Collect (and clear) everything queued so far.
pub fn take() -> Result<IncomingBatch, String> {
    handle()?
        .run_mobile_plugin::<IncomingBatch>("takePendingIntents", ())
        .map_err(|e| format!("could not read incoming shares: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kotlin_queue_shape_parses() {
        let batch: IncomingBatch = serde_json::from_value(serde_json::json!({
            "intents": [
                { "kind": "share", "text": "look", "subject": "", "paths": ["/c/a.png"], "failed": 0 },
                { "kind": "open_chat", "sessionId": "s1" }
            ],
            "copying": true
        }))
        .unwrap();
        assert!(batch.copying);
        assert_eq!(
            batch.intents[1],
            Incoming::OpenChat {
                session_id: "s1".into()
            }
        );
        let Incoming::Share { paths, .. } = &batch.intents[0] else {
            panic!()
        };
        assert_eq!(paths, &["/c/a.png"]);
    }
}
