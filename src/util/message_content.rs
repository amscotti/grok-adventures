//! Chat-completion `content` that may be a string or a list of text parts.
//!
//! Grok 4.7 still speaks the chat-completions protocol, but message content
//! can arrive as a plain string or as an array of `{ "type": "text", "text" }`
//! parts. Treating only `Option<String>` makes a valid reply fail the turn.

use serde::Deserialize;
use serde::de::Deserializer;
use serde_json::Value;

/// Deserialize assistant `content` into plain text.
pub fn deserialize_message_content<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(content_to_string))
}

fn content_to_string(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.as_str() {
                    out.push_str(text);
                } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_string),
        other => {
            let text = other.to_string();
            if text.is_empty() { None } else { Some(text) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Msg {
        #[serde(default, deserialize_with = "deserialize_message_content")]
        content: Option<String>,
    }

    #[test]
    fn accepts_string_null_and_text_parts() {
        let msg: Msg = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
        assert_eq!(msg.content.as_deref(), Some("hi"));

        let msg: Msg = serde_json::from_str(r#"{"content":null}"#).unwrap();
        assert!(msg.content.is_none());

        let msg: Msg = serde_json::from_str(r#"{}"#).unwrap();
        assert!(msg.content.is_none());

        let msg: Msg = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"Hello "},{"type":"text","text":"there"}]}"#,
        )
        .unwrap();
        assert_eq!(msg.content.as_deref(), Some("Hello there"));
    }
}
