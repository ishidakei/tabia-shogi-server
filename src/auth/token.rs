//! Token generation, hashing, and verification.
//!
//! **A stored token is a plain SHA-256, not a password hash.**
//! That is deliberate, and it is the opposite of
//! the right answer for a user-chosen password: a token is 256 bits of OS
//! entropy, so there is no dictionary to run against it and no low entropy for
//! a work factor to defend. What argon2 or bcrypt would add instead is
//! per-verification CPU cost on the CSA login path, which is a hot path when a
//! hundred engines reconnect after a restart.
//!
//! The obligations that do apply are enforced here, each by a specific piece
//! of this module:
//!
//! - The plaintext is shown once, at issuance, and is not recoverable
//!   afterwards — only [`TokenHash`] is ever persisted.
//! - The textual form is **exactly 64 characters**, matching the `open`-mode
//!   token bound (Q5), so one client
//!   configuration works against either instance type.
//! - [`Debug`] is hand-written on [`Token`] and prints a placeholder
//!   (invariant 8: no credential material in a rendering), so a credential cannot
//!   reach a log through a derived `Debug` on some later struct that happens
//!   to hold one. There is no [`Display`](std::fmt::Display) either.
//! - [`verify`] compares in constant time, so verification cannot leak the
//!   stored hash by timing.
//!
//! Nothing here validates a charset. An issued token is hex; an `open`-mode
//! token is printable ASCII of 1–64 characters; [`hash`] is total over both,
//! because the parser in `session/login.rs` is what decides which of them a
//! `LOGIN` line may carry. A second opinion about that here would be one this
//! module has no way to report.

use std::fmt;

use rand::TryRng;
use rand::rngs::SysRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The digest size, and so the stored size: SHA-256 is 32 bytes.
const HASH_BYTES: usize = 32;

/// The entropy in one issued token, in bytes — 256 bits.
const TOKEN_BYTES: usize = 32;

/// Lowercase, because the textual form is fixed as lowercase hex and an
/// uppercase digit would be a different string to every caller that compares
/// or stores one.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// An issued token in its textual form: the credential itself.
///
/// Shown to a user exactly once, at issuance, and never stored — what the
/// server keeps is the [`TokenHash`].
///
/// The value is 256 bits from the OS entropy source rendered as **lowercase
/// hex, exactly 64 characters** ([`Token::TEXT_LEN`]). The textual form is
/// bounded at 64 to match the `open`-mode token bound, and both hex (64) and
/// base64url (43) fit; hex is chosen because it
/// needs no additional crate and lands exactly on the bound, so "the longest
/// token a client must be able to carry" and "what this server issues" are one
/// number and cannot drift apart.
///
/// Three absences are deliberate:
///
/// - No derived [`Debug`]. The hand-written one below prints a placeholder.
/// - No [`Display`](std::fmt::Display). Formatting is not a way out; the only
///   door is [`reveal`](Token::reveal), whose name is the audit trail — a grep
///   for it lists every place a plaintext escapes this type.
/// - No [`Clone`]. A credential with one owner is one fewer copy to reason
///   about, and no caller needs a second.
pub struct Token(String);

impl Token {
    /// The exact length of the textual form, in characters and in bytes —
    /// 64, hex being ASCII.
    pub const TEXT_LEN: usize = 2 * TOKEN_BYTES;

    /// The token as text.
    ///
    /// Named for what it does. Every call is a place where a credential
    /// leaves the type that protects it, and there should be very few: the
    /// hash taken at issuance, and the page that shows the value once.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

/// Hand-written, per invariant 8: no credential material in a rendering.
///
/// The failure mode this defends against is not this type being logged
/// directly. It is a later struct that holds a token and derives `Debug`, at
/// which point the credential is in the logs with nothing in that struct's
/// source to suggest it.
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// The SHA-256 of a token: the only form ever persisted.
///
/// Ordinary derives here, and that is not invariant 8 being bent. The
/// invariant is about token *material*; a database read alone yields nothing
/// usable, which is the entire point of storing a hash. Redacting it would
/// make the logs less useful for no gain, and would suggest to a reader that a
/// stored hash is a secret of the same order as the token, which it is not.
///
/// No hex is written or parsed here: whether the column is a blob or a hex
/// string is `storage`'s decision, and an encoder in `auth` would be a second
/// textual form of the same value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TokenHash([u8; HASH_BYTES]);

impl TokenHash {
    /// A hash as read back from storage.
    pub const fn from_bytes(bytes: [u8; HASH_BYTES]) -> Self {
        Self(bytes)
    }

    /// The bytes, for the storage layer to persist however it persists them.
    pub const fn as_bytes(&self) -> &[u8; HASH_BYTES] {
        &self.0
    }
}

/// Issues a token: the plaintext to show once, and the hash to store.
///
/// The hash is taken through [`hash`] on the token's own text rather than over
/// the raw bytes, so issuance and verification cannot come to hash different
/// things.
///
/// # Panics
///
/// If the OS entropy source cannot produce 32 bytes. The OS RNG is required —
/// a non-cryptographic RNG here is a vulnerability — so
/// there is no weaker fallback to reach for, and a server that cannot obtain
/// entropy must not issue a credential. Nothing reachable from client input
/// calls this: issuance happens on C-3's authenticated page.
pub fn generate() -> (Token, TokenHash) {
    let mut bytes = [0u8; TOKEN_BYTES];

    // `rand` 0.10 renamed `OsRng` to `SysRng` and re-exports it from
    // `getrandom`; it is the same OS entropy source under a new name.
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("the OS entropy source is unavailable, so no token can be issued");

    let token = Token(to_hex(&bytes));
    let stored = hash(token.reveal());
    (token, stored)
}

/// Hashes a presented token.
///
/// Total: any `&str` hashes, whether it is an issued 64-character hex token or
/// an `open`-mode token of printable ASCII. Deciding which is acceptable is
/// the login parser's job.
pub fn hash(presented: &str) -> TokenHash {
    TokenHash(Sha256::digest(presented.as_bytes()).into())
}

/// Whether a presented token hashes to the stored hash. Constant-time.
///
/// The comparison goes through [`subtle::ConstantTimeEq`] and never `==`.
/// A `==` over the bytes short-circuits at the first difference, so how long
/// it takes reveals how long a prefix matched, and a stored hash can be
/// reconstructed byte by byte from enough attempts. `TokenHash` derives
/// `PartialEq` for use as a map key and in tests; this path deliberately does
/// not use it.
///
/// The presented string is hashed *first*, so the comparison is always over
/// two fixed-width digests and a truncated token cannot match as a prefix.
pub fn verify(presented: &str, stored: &TokenHash) -> bool {
    // `subtle` implements `ConstantTimeEq` for slices, not for arrays; both
    // sides are the same fixed 32 bytes, so its length check cannot branch on
    // secret data.
    hash(presented).0[..].ct_eq(&stored.0[..]).into()
}

/// Renders bytes as lowercase hex.
///
/// Two nibble lookups per byte rather than `write!`: writing into a `String`
/// yields a `fmt::Result` that would then have to be discarded, and discarding
/// a `Result` without a stated reason is forbidden here. This is infallible, so
/// there is nothing to explain away.
fn to_hex(bytes: &[u8; TOKEN_BYTES]) -> String {
    let mut text = String::with_capacity(2 * TOKEN_BYTES);
    for byte in bytes {
        text.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        text.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4 SHA-256 of the empty message.
    const SHA256_OF_EMPTY: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// FIPS 180-4 SHA-256 of `"abc"`.
    const SHA256_OF_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn hex_of(digest: &TokenHash) -> String {
        let mut text = String::new();
        for byte in digest.as_bytes() {
            text.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
            text.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
        }
        text
    }

    #[test]
    fn a_generated_tokens_hash_is_the_hash_of_its_own_text() {
        let (token, stored) = generate();

        assert_eq!(hash(token.reveal()), stored);
    }

    #[test]
    fn two_generated_tokens_differ() {
        let (first, first_hash) = generate();
        let (second, second_hash) = generate();

        assert_ne!(first.reveal(), second.reveal());
        assert_ne!(first_hash, second_hash);
    }

    #[test]
    fn a_generated_token_is_sixty_four_lowercase_hex_characters() {
        let (token, _) = generate();
        let text = token.reveal();

        assert_eq!(text.len(), 64);
        assert_eq!(text.len(), Token::TEXT_LEN);
        assert!(
            text.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "not lowercase hex: {text}"
        );
    }

    #[test]
    fn verify_accepts_a_generated_token_against_its_own_hash() {
        let (token, stored) = generate();

        assert!(verify(token.reveal(), &stored));
    }

    #[test]
    fn verify_rejects_a_different_token() {
        let (_, stored) = generate();
        let (other, _) = generate();

        assert!(!verify(other.reveal(), &stored));
    }

    #[test]
    fn verify_rejects_a_truncated_token() {
        let (token, stored) = generate();
        let truncated = &token.reveal()[..Token::TEXT_LEN - 1];

        assert!(!verify(truncated, &stored));
    }

    #[test]
    fn verify_rejects_the_empty_string() {
        let (_, stored) = generate();

        assert!(!verify("", &stored));
    }

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(hash("a presented token"), hash("a presented token"));
        assert_ne!(hash("a presented token"), hash("a presented tokem"));
    }

    #[test]
    fn hash_matches_the_published_sha256_vectors() {
        assert_eq!(hex_of(&hash("")), SHA256_OF_EMPTY);
        assert_eq!(hex_of(&hash("abc")), SHA256_OF_ABC);
    }

    #[test]
    fn hash_accepts_an_open_mode_token() {
        // Q5's `open`-mode form: printable ASCII, 1-64 characters, and no hex
        // about it. Hashing is total; the charset is the login parser's rule.
        let stored = hash("test-engine_01");

        assert!(verify("test-engine_01", &stored));
        assert!(!verify("test-engine_02", &stored));
    }

    #[test]
    fn debug_prints_a_placeholder_and_no_part_of_the_value() {
        let (token, _) = generate();
        let printed = format!("{token:?}");

        assert_eq!(printed, "Token(<redacted>)");
        for length in (4..=Token::TEXT_LEN).step_by(4) {
            assert!(!printed.contains(&token.reveal()[..length]));
        }
    }

    #[test]
    fn a_token_hash_round_trips_through_its_bytes() {
        let stored = hash("a presented token");

        assert_eq!(TokenHash::from_bytes(*stored.as_bytes()), stored);
    }
}
