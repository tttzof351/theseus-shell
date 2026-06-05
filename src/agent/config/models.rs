pub(crate) const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub(crate) const DEFAULT_MODEL: &str = "openrouter/free";
pub(crate) const DEFAULT_MAX_COMPLETION_TOKENS: usize = 16_384;
pub(crate) const DEFAULT_MAX_AGENT_TURNS: usize = 512;
pub(crate) const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 32 * 1024;
pub(crate) const DEFAULT_MAX_TOOL_BASH_BYTES: usize = 8 * 1024;
pub(crate) const DEFAULT_MAX_CONTEXT_TOKENS: usize = 200_000;
pub(crate) const DEFAULT_COMPACT_TOKEN_OVERDRAFT: usize = 8192;
pub(crate) const DEFAULT_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES: usize = 32 * 1024;
pub(crate) const DEFAULT_COMPACT_TRIM_RETRY_LIMIT: usize = 8;
pub(crate) const DEFAULT_MAX_RESUME_TRAJ: usize = 100;
pub(crate) const DEFAULT_TMP_FILES_TTL_MIN: usize = 24 * 60;
pub(crate) const DEFAULT_LLM_REQUEST_RETRIES: usize = 3;
pub(crate) const DEFAULT_LLM_REQUEST_TIMEOUT_SECONDS: usize = 180;
pub(crate) const DEFAULT_LLM_CONNECT_TIMEOUT_SECONDS: usize = 30;

pub(super) const AVAILABLE_MODELS: &[&str] = &["openrouter/free", "openai/gpt-oss-120b:free"];
