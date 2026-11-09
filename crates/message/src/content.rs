use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Image {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

// Maximum embedded image size in bytes.
const IMAGE_MAX: usize = 3_750_000;

impl Image {
    /// Supported image formats, identified by their magic bytes.
    pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some("image/jpeg")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("image/gif")
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some("image/webp")
        } else {
            None
        }
    }

    /// `bytes` as an image a model can be sent, or why not: unsupported format or size.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        use base64::Engine;
        let media_type = Self::media_type(bytes).ok_or("not a PNG, JPEG, GIF or WebP image")?;
        if bytes.len() > IMAGE_MAX {
            return Err(format!(
                "a {media_type} of {} bytes, over the {IMAGE_MAX}-byte image limit",
                bytes.len()
            ));
        }
        Ok(Self::Base64 {
            media_type: media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub content: Vec<ReasoningContent>,
    /// Origin model identifier; absent when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ReasoningContent {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Encrypted(String),
}

impl Reasoning {
    /// The prose halves, joined.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                ReasoningContent::Text { text, .. } => Some(text.as_str()),
                ReasoningContent::Encrypted(_) => None,
            })
            .collect()
    }
}
