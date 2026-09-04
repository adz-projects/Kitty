use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum MessageRole {
    #[default]
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub role: MessageRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}

impl Message {
    pub fn new(
        session_id: impl Into<String>,
        role: MessageRole,
        content: impl Into<Value>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
            created_at: Some(Utc::now()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: None,
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            status: "active".into(),
            metadata: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization() {
        let msg = Message {
            id: "msg1".into(),
            session_id: "sess1".into(),
            role: MessageRole::User,
            content: Some(Value::String("hello".into())),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
            created_at: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));
    }

    #[test]
    fn test_message_roundtrip() {
        let msg = Message::new(
            "sess1",
            MessageRole::Assistant,
            Value::String("reply".into()),
        );
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, msg.id);
        assert_eq!(parsed.role, MessageRole::Assistant);
    }

    #[test]
    fn test_session_new() {
        let s = Session::new();
        assert_eq!(s.status, "active");
        assert!(!s.id.is_empty());
    }

    #[test]
    fn test_message_role_serde() {
        let json = serde_json::to_string(&MessageRole::Tool).unwrap();
        assert_eq!(json, "\"tool\"");
        let parsed: MessageRole = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(parsed, MessageRole::Assistant);
    }
}
