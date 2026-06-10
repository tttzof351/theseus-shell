use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ChatMessage {
    pub(super) role: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_message_content"
    )]
    pub(super) content: Option<MessageContent>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_content"
    )]
    pub(super) reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub(super) enum MessageContent {
    Text(String),
    Multipart(Vec<MessageContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum MessageContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlContent },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ImageUrlContent {
    pub(super) url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ToolCall {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) kind: String,
    pub(super) function: ToolFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ToolFunctionCall {
    pub(super) name: String,
    #[serde(deserialize_with = "deserialize_tool_arguments")]
    pub(super) arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct ChatUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) is_byok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cost_details: Option<CostDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct PromptTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_write_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) audio_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) video_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct CompletionTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reasoning_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) image_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) audio_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct CostDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) upstream_inference_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) upstream_inference_prompt_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) upstream_inference_completions_cost: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatResponse {
    pub(super) choices: Vec<ChatChoice>,
    pub(super) usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatChoice {
    pub(super) message: ChatMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct TrajectoryConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum TrajectoryMessage {
    Config {
        config: TrajectoryConfig,
    },
    Chat {
        #[serde(flatten)]
        message: ChatMessage,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Box<ChatUsage>>,
    },
}

impl ChatMessage {
    pub(super) fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(MessageContent::Text(content.into())),
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub(super) fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(MessageContent::Text(content.into())),
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub(super) fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(MessageContent::Text(content.into())),
            reasoning: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub(super) fn tool_multimodal(
        tool_call_id: impl Into<String>,
        text: impl Into<String>,
        image_url: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(MessageContent::Multipart(vec![
                MessageContentPart::Text { text: text.into() },
                MessageContentPart::ImageUrl {
                    image_url: ImageUrlContent {
                        url: image_url.into(),
                    },
                },
            ])),
            reasoning: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub(super) fn content_text(&self) -> Option<String> {
        self.content.as_ref().and_then(MessageContent::text)
    }
}

impl MessageContent {
    pub(super) fn text(&self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text.clone()),
            Self::Multipart(parts) => {
                let text = parts
                    .iter()
                    .filter_map(MessageContentPart::text)
                    .collect::<Vec<_>>()
                    .join("");

                if text.is_empty() { None } else { Some(text) }
            }
        }
    }
}

impl MessageContentPart {
    fn text(&self) -> Option<String> {
        match self {
            Self::Text { text } => Some(text.clone()),
            Self::ImageUrl { .. } => None,
        }
    }
}

impl TrajectoryMessage {
    pub(super) fn new(message: ChatMessage) -> Self {
        Self::Chat {
            message,
            usage: None,
        }
    }

    pub(super) fn with_usage(message: ChatMessage, usage: Option<ChatUsage>) -> Self {
        Self::Chat {
            message,
            usage: usage.map(Box::new),
        }
    }

    pub(super) fn config(model: Option<String>) -> Self {
        Self::Config {
            config: TrajectoryConfig { model },
        }
    }

    pub(super) fn message(&self) -> Option<&ChatMessage> {
        match self {
            Self::Chat { message, .. } => Some(message),
            Self::Config { .. } => None,
        }
    }

    pub(super) fn usage(&self) -> Option<&ChatUsage> {
        match self {
            Self::Chat { usage, .. } => usage.as_deref(),
            Self::Config { .. } => None,
        }
    }
}

fn deserialize_optional_message_content<'de, D>(
    deserializer: D,
) -> Result<Option<MessageContent>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(content_value_to_message_content))
}

fn content_value_to_message_content(value: Value) -> Option<MessageContent> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(MessageContent::Text(text)),
        Value::Array(parts) => serde_json::from_value(Value::Array(parts)).ok(),
        other => Some(MessageContent::Text(other.to_string())),
    }
}

fn deserialize_optional_content<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(content_value_to_string))
}

fn content_value_to_string(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text),
        Value::Array(parts) => {
            let text = parts
                .into_iter()
                .filter_map(content_part_to_string)
                .collect::<Vec<_>>()
                .join("");

            if text.is_empty() { None } else { Some(text) }
        }
        other => Some(other.to_string()),
    }
}

fn content_part_to_string(value: Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text),
        Value::Object(mut object) => object
            .remove("text")
            .and_then(content_value_to_string)
            .or_else(|| object.remove("content").and_then(content_value_to_string)),
        _ => None,
    }
}

fn deserialize_tool_arguments<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(match value {
        Value::String(text) => text,
        other => other.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_content_parts_response() {
        let text = r#"
        {
          "choices": [
            {
              "message": {
                "role": "assistant",
                "content": [
                  { "type": "text", "text": "hello" },
                  { "type": "text", "text": " world" }
                ]
              }
            }
          ]
        }
        "#;

        let response = serde_json::from_str::<ChatResponse>(text).unwrap();

        assert_eq!(
            response.choices[0].message.content_text().as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn serializes_tool_multimodal_content() {
        let message = ChatMessage::tool_multimodal(
            "call_1",
            "Read image image.png",
            "data:image/png;base64,abc",
        );

        let value = serde_json::to_value(message).unwrap();

        assert_eq!(value.get("role").and_then(Value::as_str), Some("tool"));
        assert_eq!(
            value.get("tool_call_id").and_then(Value::as_str),
            Some("call_1")
        );
        let content = value.get("content").and_then(Value::as_array).unwrap();
        assert_eq!(content[0].get("type").and_then(Value::as_str), Some("text"));
        assert_eq!(
            content[1].get("type").and_then(Value::as_str),
            Some("image_url")
        );
        assert_eq!(
            content[1]
                .get("image_url")
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str),
            Some("data:image/png;base64,abc")
        );
    }

    #[test]
    fn decodes_object_tool_arguments() {
        let text = r#"
        {
          "id": "tool-1",
          "type": "function",
          "function": {
            "name": "bash",
            "arguments": { "command": "ls" }
          }
        }
        "#;

        let tool_call = serde_json::from_str::<ToolCall>(text).unwrap();

        assert_eq!(tool_call.function.arguments, "{\"command\":\"ls\"}");
    }

    #[test]
    fn decodes_openrouter_reasoning_field() {
        let text = r#"
        {
          "choices": [
            {
              "message": {
                "role": "assistant",
                "content": null,
                "reasoning": "I should inspect the file before answering.",
                "tool_calls": [
                  {
                    "id": "call_1",
                    "type": "function",
                    "function": {
                      "name": "read_file",
                      "arguments": "{\"path\":\"README.md\"}"
                    }
                  }
                ]
              }
            }
          ]
        }
        "#;

        let response = serde_json::from_str::<ChatResponse>(text).unwrap();

        assert_eq!(
            response.choices[0].message.reasoning.as_deref(),
            Some("I should inspect the file before answering.")
        );
    }

    #[test]
    fn decodes_openrouter_usage() {
        let text = r#"
        {
          "choices": [
            {
              "message": {
                "role": "assistant",
                "content": "done"
              }
            }
          ],
          "usage": {
            "prompt_tokens": 387,
            "completion_tokens": 53,
            "total_tokens": 440,
            "cost": 0,
            "is_byok": false,
            "prompt_tokens_details": {
              "cached_tokens": 64,
              "cache_write_tokens": 0,
              "audio_tokens": 0,
              "video_tokens": 0
            },
            "cost_details": {
              "upstream_inference_cost": 0,
              "upstream_inference_prompt_cost": 0,
              "upstream_inference_completions_cost": 0
            },
            "completion_tokens_details": {
              "reasoning_tokens": 20,
              "image_tokens": 0,
              "audio_tokens": 0
            }
          }
        }
        "#;

        let response = serde_json::from_str::<ChatResponse>(text).unwrap();
        let usage = response.usage.unwrap();

        assert_eq!(usage.prompt_tokens, Some(387));
        assert_eq!(usage.completion_tokens, Some(53));
        assert_eq!(usage.total_tokens, Some(440));
        assert_eq!(usage.cost, Some(0.0));
        assert_eq!(usage.is_byok, Some(false));
        assert_eq!(
            usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            Some(64)
        );
        assert_eq!(
            usage
                .completion_tokens_details
                .as_ref()
                .and_then(|details| details.reasoning_tokens),
            Some(20)
        );
        assert_eq!(
            usage
                .cost_details
                .as_ref()
                .and_then(|details| details.upstream_inference_cost),
            Some(0.0)
        );
    }

    #[test]
    fn trajectory_message_can_include_completion_usage() {
        let trajectory_message = TrajectoryMessage::with_usage(
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text("done".to_string())),
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Some(ChatUsage {
                prompt_tokens: Some(1),
                completion_tokens: Some(2),
                total_tokens: Some(3),
                prompt_tokens_details: None,
                completion_tokens_details: None,
                cost: Some(0.0),
                is_byok: Some(false),
                cost_details: None,
            }),
        );

        let serialized = serde_json::to_string(&trajectory_message).unwrap();

        assert!(serialized.contains("\"role\":\"assistant\""));
        assert!(serialized.contains("\"usage\""));
        assert!(serialized.contains("\"total_tokens\":3"));
    }
}
