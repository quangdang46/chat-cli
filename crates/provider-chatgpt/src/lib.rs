pub mod pow;
pub mod protocol;

use chat_core::provider::{ChatReq, ChatResp, Provider, ProviderHandle, Session};

// TODO: implement ChatGPT web provider
// - auth(): GET /api/auth/session with session_token, return Session + cache access_token
// - chat(): handle sentinel PoW via `pow::SolvePow` + POST /backend-api/conversation
pub struct ChatGptProvider;

impl Provider for ChatGptProvider {
    fn id(&self) -> &'static str {
        "chatgpt"
    }

    fn context_limit(&self) -> usize {
        // ~128k tokens ≈ 512k chars
        512_000
    }

    fn auth(&self, _token: &str) -> anyhow::Result<Session> {
        // TODO: GET https://chatgpt.com/api/auth/session
        // Cookie: __Secure-next-auth.session-token=<token>
        // Validate response: { accessToken, user, expires }
        anyhow::bail!("chatgpt auth not yet implemented")
    }

    fn chat(&self, _handle: &ProviderHandle, _req: ChatReq) -> anyhow::Result<ChatResp> {
        // TODO:
        // 1. Ensure access_token valid (refresh via /api/auth/session if needed)
        // 2. POST /backend-api/sentinel/chat-requirements/finalize (if required) + PoW
        // 3. POST /backend-api/conversation with conversation_id + parent_message_id
        anyhow::bail!("chatgpt chat not yet implemented")
    }
}

inventory::submit!(chat_core::provider::ProviderEntry {
    id: "chatgpt",
    factory: || Box::new(ChatGptProvider),
});
