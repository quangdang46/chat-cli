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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_limit_passes() {
        let result = budget_check(Some("sys"), "history", "attachments", "prompt", 10_000);
        assert!(result.is_ok());
    }

    #[test]
    fn exactly_at_limit_passes() {
        // total = 5 + 5 = 10 == limit → not exceeded
        let result = budget_check(None, "12345", "", "67890", 10);
        assert!(
            result.is_ok(),
            "total == limit must pass (strictly greater fails)"
        );
    }

    #[test]
    fn one_over_limit_fails() {
        let result = budget_check(None, "", "", "123456", 5);
        let err = result.unwrap_err();
        assert_eq!(err.limit, 5);
        assert_eq!(err.total, 6);
        assert_eq!(err.prompt_len, 6);
    }

    #[test]
    fn system_none_counts_as_zero() {
        let err = budget_check(None, "", "", "abc", 2).unwrap_err();
        assert_eq!(err.system_len, 0);
        assert_eq!(err.total, 3);
    }

    #[test]
    fn over_limit_error_has_full_breakdown() {
        let err = budget_check(
            Some("system-prompt"),
            "history-text",
            "attachment-text",
            "the-prompt",
            10,
        )
        .unwrap_err();

        assert_eq!(err.system_len, "system-prompt".len());
        assert_eq!(err.history_len, "history-text".len());
        assert_eq!(err.attachments_len, "attachment-text".len());
        assert_eq!(err.prompt_len, "the-prompt".len());

        let expected_total = "system-prompt".len()
            + "history-text".len()
            + "attachment-text".len()
            + "the-prompt".len();
        assert_eq!(err.total, expected_total);

        let msg = err.to_string();
        assert!(
            msg.contains(&format!("context limit {} exceeded", 10)),
            "message should name the limit: {msg}"
        );
        for segment in [
            format!("system={}", "system-prompt".len()),
            format!("history={}", "history-text".len()),
            format!("attachments={}", "attachment-text".len()),
            format!("prompt={}", "the-prompt".len()),
            format!("total={expected_total}"),
        ] {
            assert!(
                msg.contains(&segment),
                "breakdown missing '{segment}': {msg}"
            );
        }
        assert!(
            msg.contains("--new"),
            "actionable hint to reduce input should be present: {msg}"
        );
    }

    #[test]
    fn empty_everything_under_any_positive_limit_passes() {
        assert!(budget_check(None, "", "", "", 1).is_ok());
    }
}
