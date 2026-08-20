//! PoW for ChatGPT sentinel — ported from ds2api/pow (DeepSeekHashV1).
//!
//! DeepSeekHashV1 = SHA3-256 with Keccak-f[1600] skipping round 0 (rounds 1..23).
//! If ChatGPT sentinel uses the same hash, this module is shared;
//! otherwise it will be adapted to ChatGPT's challenge format.
//!
//! Reference: ds2api/pow/deepseek_hash.go + deepseek_pow.go

// TODO: port keccakF23 (rounds 1..23) and SolvePow from ds2api/pow
// Keep the same structure:
//   - BuildPrefix(salt, expireAt) -> "salt_expireAt_"
//   - SolvePow(ctx, challengeHex, salt, expireAt, difficulty) -> nonce
//   - BuildPowHeader(challenge, answer) -> base64(JSON{...})
// Preserve: pre-absorb prefix, zero-alloc nonce loop, ctx cancel every 1024 iter.

/// Placeholder — to be implemented by porting ds2api/pow.
pub fn solve_pow(
    _challenge_hex: &str,
    _salt: &str,
    _expire_at: i64,
    _difficulty: i64,
) -> anyhow::Result<i64> {
    anyhow::bail!("PoW not yet implemented — port from ds2api/pow")
}
