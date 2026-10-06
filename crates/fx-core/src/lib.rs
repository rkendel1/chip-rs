use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelId(pub String);

impl ModelId {
    pub fn new(model: impl Into<String>) -> Self {
        Self(model.into())
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

impl Message {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    pub model: ModelId,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl ModelRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: ModelId::new(model),
            messages,
            max_tokens: Some(256),
            temperature: Some(0.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl Usage {
    pub fn new(prompt_tokens: u32, completion_tokens: u32) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelResponse {
    pub id: String,
    pub output: String,
    pub usage: Usage,
}

impl ModelResponse {
    pub fn new(id: impl Into<String>, output: impl Into<String>, usage: Usage) -> Self {
        Self {
            id: id.into(),
            output: output.into(),
            usage,
        }
    }
}

/// A secret string (e.g. an API key). Never shown by `Debug`; read it only via `expose`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FxError {
    InvalidRequest(String),
    Provider(String),
    Configuration(String),
    Authentication(String),
    Http(String),
    InvalidResponse(String),
    Timeout(String),
    Serialization(String),
}

impl fmt::Display for FxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid request: {message}"),
            Self::Provider(message) => write!(f, "provider error: {message}"),
            Self::Configuration(message) => write!(f, "configuration error: {message}"),
            Self::Authentication(message) => write!(f, "authentication failure: {message}"),
            Self::Http(message) => write!(f, "http failure: {message}"),
            Self::InvalidResponse(message) => write!(f, "invalid response: {message}"),
            Self::Timeout(message) => write!(f, "timeout: {message}"),
            Self::Serialization(message) => write!(f, "serialization error: {message}"),
        }
    }
}

impl std::error::Error for FxError {}

#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError>;
}
