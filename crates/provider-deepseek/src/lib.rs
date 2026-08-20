pub mod protocol;

use chat_core::provider::{ChatReq, ChatResp, Provider, ProviderHandle, Session};
use inventory;

// TODO: implement DeepSeek web provider
// - auth(): validate token via DeepSeek API
// - chat(): create_chat_session if --new, then POST /api/v0/chat/completion with PoW header
pub struct DeepSeekProvider;

impl Provider for DeepSeekProvider {
    fn id(&self) -> &'static str {
        "deepseek"
    }

    fn context_limit(&self) -> usize {
        // ~64k tokens ≈ 256k chars
        256_000
    }

    fn auth(&self, _token: &str) -> anyhow::Result<Session> {
        // TODO: validate DeepSeek token
        anyhow::bail!("deepseek auth not yet implemented")
    }

    fn chat(&self, _handle: &ProviderHandle, _req: ChatReq) -> anyhow::Result<ChatResp> {
        // TODO:
        // 1. If handle.conversation_id is None -> POST /api/v0/chat_session/create
        // 2. GET /api/v0/chat/create_pow_challenge -> solve PoW
        // 3. POST /api/v0/chat/completion with x-ds-pow-response header
        anyhow::bail!("deepseek chat not yet implemented")
    }
}

inventory::submit!(chat_core::provider::ProviderEntry {
    id: "deepseek",
    factory: || Box::new(DeepSeekProvider),
});
