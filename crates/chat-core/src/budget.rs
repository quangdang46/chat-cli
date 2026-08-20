//! Budget check: fail fast with breakdown, no auto-truncation.
//!
//! Checked at the last point before `Provider::chat()`:
//!   total = system + history_text + attachments_text + prompt

use thiserror::Error;

#[derive(Debug, Error)]
#[error("context limit {limit} exceeded: system={system_len} history={history_len} attachments={attachments_len} prompt={prompt_len} total={total} — reduce input or use --new to start fresh")]
pub struct BudgetError {
    pub limit: usize,
    pub system_len: usize,
    pub history_len: usize,
    pub attachments_len: usize,
    pub prompt_len: usize,
    pub total: usize,
}

/// Approximate chars (POC uses char count; token-aware later).
pub fn budget_check(
    system: Option<&str>,
    history_text: &str,
    attachments_text: &str,
    prompt: &str,
    limit: usize,
) -> Result<(), BudgetError> {
    let system_len = system.map(|s| s.len()).unwrap_or(0);
    let history_len = history_text.len();
    let attachments_len = attachments_text.len();
    let prompt_len = prompt.len();
    let total = system_len + history_len + attachments_len + prompt_len;
    // Approx: 1 token ≈ 4 chars, but POC checks chars directly against limit (chars).
    // limit is context_limit in chars (e.g. 128k * 4 = 512k chars).
    // For POC we compare chars vs chars*4 to be lenient — use limit as chars.
    if total > limit {
        return Err(BudgetError {
            limit,
            system_len,
            history_len,
            attachments_len,
            prompt_len,
            total,
        });
    }
    Ok(())
}
