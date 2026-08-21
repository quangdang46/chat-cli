//! Sentinel PoW solver — Keccak-f[1600] with round 0 skipped (rounds 1..23).
//!
//! `DeepSeekHashV1` = SHA3-256 family variant: rate 136 bytes, domain padding
//! `0x06 … 0x80`, but the permutation runs only rounds 1..23. This is the hash
//! used by DeepSeek's `/chat/create_pow_challenge` and shared by ChatGPT's
//! sentinel challenge (same construction, see plan §5.6).
//!
//! Ported faithfully from `ds2api/pow` (`deepseek_hash.go`, `deepseek_pow.go`);
//! the test vectors below were captured by calling DeepSeek's official WASM.
//!
//! Preserved reference optimizations:
//!   - pre-absorb `prefix = "{salt}_{expire_at}_"` into the Keccak state once,
//!   - zero-heap nonce suffix per iteration (fixed 20-byte ASCII buffer),
//!   - cooperative cancellation checked every 1024 nonces.

use base64::Engine;

/// Round constants for Keccak-f[1600] (index 0 is unused — round 0 skipped).
const RC: [u64; 24] = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808A,
    0x8000000080008000,
    0x000000000000808B,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008A,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000A,
    0x000000008000808B,
    0x800000000000008B,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800A,
    0x800000008000000A,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
];

/// Keccak-f[1600] restricted to rounds 1..=23 — mirrors ds2api `keccakF23`.
fn keccak_f23(s: &mut [u64; 25]) {
    let mut a = *s;

    for r in 1..24 {
        // θ (theta)
        let mut c = [0u64; 5];
        for x in 0..5 {
            c[x] = a[x] ^ a[x + 5] ^ a[x + 10] ^ a[x + 15] ^ a[x + 20];
        }
        let mut d = [0u64; 5];
        for x in 0..5 {
            d[x] = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
        }
        for y in 0..5 {
            for x in 0..5 {
                a[x + 5 * y] ^= d[x];
            }
        }

        // ρ (rho) + π (pi) — rotation offsets transcribed verbatim from the
        // reference implementation to guarantee bit-exactness.
        let mut b = [0u64; 25];
        b[0] = a[0];
        b[10] = a[1].rotate_left(1);
        b[20] = a[2].rotate_left(62);
        b[5] = a[3].rotate_left(28);
        b[15] = a[4].rotate_left(27);
        b[16] = a[5].rotate_left(36);
        b[1] = a[6].rotate_left(44);
        b[11] = a[7].rotate_left(6);
        b[21] = a[8].rotate_left(55);
        b[6] = a[9].rotate_left(20);
        b[7] = a[10].rotate_left(3);
        b[17] = a[11].rotate_left(10);
        b[2] = a[12].rotate_left(43);
        b[12] = a[13].rotate_left(25);
        b[22] = a[14].rotate_left(39);
        b[23] = a[15].rotate_left(41);
        b[8] = a[16].rotate_left(45);
        b[18] = a[17].rotate_left(15);
        b[3] = a[18].rotate_left(21);
        b[13] = a[19].rotate_left(8);
        b[14] = a[20].rotate_left(18);
        b[24] = a[21].rotate_left(2);
        b[9] = a[22].rotate_left(61);
        b[19] = a[23].rotate_left(56);
        b[4] = a[24].rotate_left(14);

        // χ (chi)
        for t in [0usize, 5, 10, 15, 20] {
            for j in 0..5 {
                a[t + j] = b[t + j] ^ (!b[t + (j + 1) % 5] & b[t + (j + 2) % 5]);
            }
        }

        // ι (iota) — skip rc[0]: round 0 never runs in this variant.
        a[0] ^= RC[r];
    }

    *s = a;
}

/// Absorb one full 136-byte block (already padded) and permute.
fn absorb_block(s: &mut [u64; 25], block: &[u8; 136]) {
    for i in 0..136 / 8 {
        let word = u64::from_le_bytes(block[i * 8..i * 8 + 8].try_into().unwrap());
        s[i] ^= word;
    }
    keccak_f23(s);
}

/// `DeepSeekHashV1` — 32-byte digest of `data` (equivalent to the official
/// `wasm_deepseek_hash_v1`).
pub fn deep_seek_hash_v1(data: &[u8]) -> [u8; 32] {
    const RATE: usize = 136;
    let mut s = [0u64; 25];

    let mut off = 0;
    while off + RATE <= data.len() {
        for i in 0..RATE / 8 {
            let word = u64::from_le_bytes(data[off + i * 8..off + i * 8 + 8].try_into().unwrap());
            s[i] ^= word;
        }
        keccak_f23(&mut s);
        off += RATE;
    }

    // Final partial block with 0x06 … 0x80 padding.
    let rem = data.len() - off;
    let mut block = [0u8; RATE];
    block[..rem].copy_from_slice(&data[off..]);
    block[rem] = 0x06;
    block[RATE - 1] |= 0x80;
    absorb_block(&mut s, &block);

    let mut out = [0u8; 32];
    out[0..8].copy_from_slice(&s[0].to_le_bytes());
    out[8..16].copy_from_slice(&s[1].to_le_bytes());
    out[16..24].copy_from_slice(&s[2].to_le_bytes());
    out[24..32].copy_from_slice(&s[3].to_le_bytes());
    out
}

/// `"{salt}_{expire_at}_"` — the fixed message prefix from pow.go:89.
pub fn build_prefix(salt: &str, expire_at: i64) -> String {
    format!("{}_{}_", salt, expire_at)
}

fn decode_hex32(hex_str: &str) -> Option<[u8; 32]> {
    if hex_str.len() != 64 {
        return None;
    }
    let bytes = hex_str.as_bytes();
    let digit = |b: u8| (b as char).to_digit(16);
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = digit(bytes[2 * i])?;
        let lo = digit(bytes[2 * i + 1])?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Search `nonce ∈ [0, difficulty)` such that
/// `DeepSeekHashV1(prefix + str(nonce)) == challenge` — non-cancellable form.
pub fn solve_pow(
    challenge_hex: &str,
    salt: &str,
    expire_at: i64,
    difficulty: i64,
) -> anyhow::Result<i64> {
    solve_pow_with_cancel(challenge_hex, salt, expire_at, difficulty, || false)
}

/// Cancellable solver: `should_stop` is polled every 1024 nonces
/// (mirrors the reference's `ctx.Err()` checkpoint).
pub fn solve_pow_with_cancel(
    challenge_hex: &str,
    salt: &str,
    expire_at: i64,
    difficulty: i64,
    mut should_stop: impl FnMut() -> bool,
) -> anyhow::Result<i64> {
    if challenge_hex.len() != 64 {
        anyhow::bail!("pow: challenge must be 64 hex chars");
    }
    let target = decode_hex32(challenge_hex).ok_or_else(|| {
        anyhow::anyhow!("pow: challenge is not valid 64-char lowercase/uppercase hex")
    })?;
    let t0 = u64::from_le_bytes(target[0..8].try_into().unwrap());
    let t1 = u64::from_le_bytes(target[8..16].try_into().unwrap());
    let t2 = u64::from_le_bytes(target[16..24].try_into().unwrap());
    let t3 = u64::from_le_bytes(target[24..32].try_into().unwrap());

    // Pre-absorb the fixed prefix once; only the nonce suffix varies.
    const RATE: usize = 136;
    let prefix = build_prefix(salt, expire_at).into_bytes();
    let mut base_state = [0u64; 25];
    let mut off = 0;
    while off + RATE <= prefix.len() {
        for i in 0..RATE / 8 {
            let word = u64::from_le_bytes(prefix[off + i * 8..off + i * 8 + 8].try_into().unwrap());
            base_state[i] ^= word;
        }
        keccak_f23(&mut base_state);
        off += RATE;
    }
    let tail_len = prefix.len() - off;
    let mut tail = [0u8; RATE];
    tail[..tail_len].copy_from_slice(&prefix[off..]);

    // Zero-alloc decimal rendering of the nonce (fits any i64).
    let mut num_buf = [0u8; 20];
    for n in 0..difficulty {
        // Cooperative cancellation every 1024 candidates.
        if n & 0x3FF == 0 && should_stop() {
            anyhow::bail!("pow: cancelled before a solution was found");
        }

        let mut v = n as u64;
        let mut pos = 20usize;
        if v == 0 {
            pos -= 1;
            num_buf[pos] = b'0';
        } else {
            while v > 0 {
                pos -= 1;
                num_buf[pos] = b'0' + (v % 10) as u8;
                v /= 10;
            }
        }
        let num_len = 20 - pos;

        let mut s = base_state;
        let total_tail = tail_len + num_len;
        if total_tail < RATE {
            let mut buf = [0u8; RATE];
            buf[..tail_len].copy_from_slice(&tail[..tail_len]);
            buf[tail_len..total_tail].copy_from_slice(&num_buf[pos..]);
            buf[total_tail] = 0x06;
            buf[RATE - 1] |= 0x80;
            absorb_block(&mut s, &buf);
        } else {
            // Nonce spills across a second block.
            let mut buf = [0u8; RATE];
            buf[..tail_len].copy_from_slice(&tail[..tail_len]);
            buf[tail_len..RATE].copy_from_slice(&num_buf[pos..pos + (RATE - tail_len)]);
            absorb_block(&mut s, &buf);

            let mut buf2 = [0u8; RATE];
            let rem = total_tail - RATE;
            buf2[..rem]
                .copy_from_slice(&num_buf[pos + (RATE - tail_len)..pos + (RATE - tail_len) + rem]);
            buf2[rem] = 0x06;
            buf2[RATE - 1] |= 0x80;
            absorb_block(&mut s, &buf2);
        }

        if s[0] == t0 && s[1] == t1 && s[2] == t2 && s[3] == t3 {
            return Ok(n);
        }
    }
    anyhow::bail!("pow: no solution within difficulty {}", difficulty)
}

/// One `/chat/create_pow_challenge` payload (`biz_data.challenge`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Challenge {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub expire_at: i64,
    #[serde(default)]
    pub difficulty: i64,
    #[serde(default)]
    pub signature: String,
    #[serde(default)]
    pub target_path: String,
}

/// Serialize `{algorithm,challenge,salt,answer,signature,target_path}` as
/// base64(JSON) — the value for the `x-ds-pow-response` header.
/// `difficulty`/`expire_at` are intentionally excluded (pow.go:218).
pub fn build_pow_header(challenge: &Challenge, answer: i64) -> anyhow::Result<String> {
    let payload = serde_json::json!({
        "algorithm": challenge.algorithm,
        "challenge": challenge.challenge,
        "salt": challenge.salt,
        "answer": answer,
        "signature": challenge.signature,
        "target_path": challenge.target_path,
    });
    Ok(base64::engine::general_purpose::STANDARD.encode(payload.to_string()))
}

/// End-to-end: `Challenge` → `x-ds-pow-response` header string.
/// Defaults difficulty to 144_000 when the server omits it.
pub fn solve_and_build_header(challenge: &Challenge) -> anyhow::Result<String> {
    if challenge.algorithm != "DeepSeekHashV1" {
        anyhow::bail!("pow: unsupported algorithm: {}", challenge.algorithm);
    }
    let difficulty = if challenge.difficulty == 0 {
        144_000
    } else {
        challenge.difficulty
    };
    let answer = solve_pow(
        &challenge.challenge,
        &challenge.salt,
        challenge.expire_at,
        difficulty,
    )?;
    build_pow_header(challenge, answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_hex(input: &str) -> String {
        let digest = deep_seek_hash_v1(input.as_bytes());
        digest.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Vectors produced by calling DeepSeek's official WASM (ds2api test suite).
    #[test]
    fn hash_matches_official_wasm_vectors() {
        assert_eq!(
            hash_hex(""),
            "e594808bc5b7151ac160c6d39a02e0a8e261ed588578403099e3561dc40c26b3"
        );
        assert_eq!(
            hash_hex("testsalt_1700000000_42"),
            "d4a2ea58c89e40887c933484868380c6f803eaa8dc53a3b9df8e431b921a4f09"
        );
        assert_eq!(
            hash_hex("testsalt_1700000000_100000"),
            "abea2f35796b65486e9be1b36f7878c66cab021e96faa473fdf4decd31f9ba30"
        );
        assert_eq!(
            hash_hex("abc123salt_1700000000_12345"),
            "74b3b7452745b70e85eb32ee7f0a9ec0381d42dd5137b695da915e104fc390e1"
        );
    }

    /// Sanity anchor: skipping round 0 must differ from stock SHA3-256.
    #[test]
    fn hash_differs_from_standard_sha3_256() {
        // sha3-256("") = a7ffc6…; our variant must NOT match it.
        assert_ne!(
            hash_hex(""),
            "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a"
        );
    }

    #[test]
    fn build_prefix_formats_salt_expire_trailing_underscore() {
        assert_eq!(build_prefix("testsalt", 1700000000), "testsalt_1700000000_");
    }

    #[test]
    fn solve_pow_recovers_known_nonces() {
        for (salt, expire, answer, diff) in [
            ("testsalt", 1700000000i64, 42i64, 1000i64),
            ("testsalt", 1700000000, 500, 2000),
            ("abc123salt", 1700000000, 12345, 20000),
        ] {
            let digest = deep_seek_hash_v1(format!("{}_{}_{}", salt, expire, answer).as_bytes());
            let challenge_hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
            let got = solve_pow(&challenge_hex, salt, expire, diff).unwrap_or_else(|e| {
                panic!("solve_pow failed for salt={salt} answer={answer}: {e}")
            });
            assert_eq!(got, answer, "nonce mismatch for salt={salt}");
        }
    }

    #[test]
    fn solve_pow_rejects_bad_challenge_length() {
        let err = solve_pow("deadbeef", "salt", 1, 100).unwrap_err();
        assert!(err.to_string().contains("64 hex chars"));
    }

    #[test]
    fn solve_pow_rejects_invalid_hex() {
        let err = solve_pow(&"zz".repeat(32), "salt", 1, 100).unwrap_err();
        assert!(err.to_string().contains("hex"));
    }

    #[test]
    fn solve_pow_reports_no_solution_within_tight_difficulty() {
        // Real answer is 42; searching only [0, 42) cannot find it.
        let digest = deep_seek_hash_v1(b"testsalt_1700000000_42");
        let challenge_hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
        let err = solve_pow(&challenge_hex, "testsalt", 1700000000, 42).unwrap_err();
        assert!(err.to_string().contains("no solution within difficulty"));
    }

    #[test]
    fn solve_pow_honors_cancellation_every_1024_nonces() {
        let digest = deep_seek_hash_v1(b"testsalt_1700000000_5000");
        let challenge_hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
        let mut polls = 0;
        let result = solve_pow_with_cancel(&challenge_hex, "testsalt", 1700000000, 100_000, || {
            polls += 1;
            true // stop at the very first checkpoint
        });
        assert!(result.is_err());
        assert_eq!(polls, 1, "checkpoint must fire at nonce 0");
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }

    #[test]
    fn build_pow_header_round_trip_carries_answer() {
        use base64::engine::general_purpose::STANDARD;

        let challenge = Challenge {
            algorithm: "DeepSeekHashV1".to_string(),
            challenge: "ab".repeat(32),
            salt: "salt".to_string(),
            expire_at: 1712345678,
            difficulty: 2000,
            signature: "sig".to_string(),
            target_path: "/api/v0/chat/completion".to_string(),
        };
        let header = build_pow_header(&challenge, 777).unwrap();

        let raw = STANDARD.decode(header).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(parsed["answer"], 777);
        assert_eq!(parsed["algorithm"], "DeepSeekHashV1");
        assert_eq!(parsed["salt"], "salt");
        assert_eq!(parsed["target_path"], "/api/v0/chat/completion");
        assert_eq!(parsed["signature"], "sig");
        // Excluded by design (pow.go:218).
        assert!(parsed.get("difficulty").is_none());
        assert!(parsed.get("expire_at").is_none());
    }

    #[test]
    fn solve_and_build_header_end_to_end_defaults_difficulty() {
        let digest = deep_seek_hash_v1(b"salt_1712345678_777");
        let challenge_hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();

        let challenge = Challenge {
            algorithm: "DeepSeekHashV1".to_string(),
            challenge: challenge_hex,
            salt: "salt".to_string(),
            expire_at: 1712345678,
            difficulty: 0, // server omitted → must default to 144_000
            signature: "sig".to_string(),
            target_path: "/api/v0/chat/completion".to_string(),
        };

        use base64::engine::general_purpose::STANDARD;
        let header = solve_and_build_header(&challenge).unwrap();
        let raw = STANDARD.decode(header).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(parsed["answer"], 777);
    }

    #[test]
    fn solve_and_build_header_rejects_unknown_algorithm() {
        let challenge = Challenge {
            algorithm: "SomeOtherV9".to_string(),
            challenge: "ab".repeat(32),
            salt: "s".to_string(),
            expire_at: 1,
            difficulty: 10,
            signature: String::new(),
            target_path: String::new(),
        };
        let err = solve_and_build_header(&challenge).unwrap_err();
        assert!(err.to_string().contains("unsupported algorithm"));
    }
}
