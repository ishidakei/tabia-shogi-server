//! The two primitives a cookie-borne session needs: an opaque identifier, and a
//! signature over it.
//!
//! The signature is not what makes a session unguessable: [`opaque_id`] is 256
//! bits from the OS entropy source. What the MAC buys is that the session
//! store is never asked about a value a client made up, so a tampered cookie
//! is an anonymous request rather than a miss in a map.
//!
//! HMAC-SHA256 is written out here rather than taken from an `hmac` crate.
//! RFC 2104's construction is the nine lines of [`CookieKey::mac`], the key is
//! always exactly one SHA-256 digest wide so there is no key-shortening branch
//! to get wrong, and the RFC 4231 test vectors below are what hold it.
//!
//! The key comes from the environment and never from the configuration file,
//! so [`CookieKey`] is parsed from a variable's text by `config::Secrets` and
//! nothing here reads one.

use std::fmt;

use rand::TryRng;
use rand::rngs::SysRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// SHA-256's digest size, which is both the key size and the MAC size.
const HASH_BYTES: usize = 32;

/// SHA-256's block size, which is what HMAC pads the key to.
const BLOCK_BYTES: usize = 64;

/// The entropy in one opaque identifier, in bytes — 256 bits.
const ID_BYTES: usize = 32;

/// Lowercase: an uppercase digit would be a different string to every caller
/// that compares one.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// What separates a signed cookie's value from its MAC.
///
/// A character that appears in neither half — both are lowercase hex — so the
/// split is unambiguous whatever the value is.
const SEPARATOR: char = '.';

/// The key a session cookie is signed with: 32 bytes, from the environment.
///
/// No derived [`Debug`], no [`Display`](fmt::Display), no accessor, no
/// [`Clone`]: the bytes go into [`mac`](Self::mac) and nowhere else.
pub struct CookieKey([u8; HASH_BYTES]);

/// Why a written key is not a key. Neither message quotes the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// Not [`CookieKey::TEXT_LEN`] characters.
    #[error("expected {expected} hexadecimal characters, got {got}")]
    Length {
        /// [`CookieKey::TEXT_LEN`].
        expected: usize,
        /// How many characters were written.
        got: usize,
    },

    /// The right length, but not hex.
    #[error("expected {} lowercase hexadecimal characters", CookieKey::TEXT_LEN)]
    NotHex,
}

impl CookieKey {
    /// The exact length of a written key, in characters — 64, hex being ASCII.
    pub const TEXT_LEN: usize = 2 * HASH_BYTES;

    /// The key an operator wrote into the environment.
    ///
    /// Lowercase hex of exactly [`TEXT_LEN`](Self::TEXT_LEN) characters, which
    /// is the form `openssl rand -hex 32` produces.
    ///
    /// # Errors
    ///
    /// [`KeyError`], naming the shape that was expected and never the value.
    pub fn parse(written: &str) -> Result<Self, KeyError> {
        if written.len() != Self::TEXT_LEN {
            return Err(KeyError::Length {
                expected: Self::TEXT_LEN,
                got: written.chars().count(),
            });
        }

        let mut key = [0u8; HASH_BYTES];
        if !from_hex(written, &mut key) {
            return Err(KeyError::NotHex);
        }

        Ok(Self(key))
    }

    /// `value`, with its MAC appended: what goes into the cookie.
    ///
    /// The value travels in the clear: a session id is opaque, and the server
    /// has to read it back to look the session up.
    pub fn signed(&self, value: &str) -> String {
        let mut cookie = String::with_capacity(value.len() + 1 + Self::TEXT_LEN);
        cookie.push_str(value);
        cookie.push(SEPARATOR);
        cookie.push_str(&to_hex(&self.mac(value.as_bytes())));

        cookie
    }

    /// The value inside a cookie this key signed, or [`None`].
    ///
    /// `None` covers every way a cookie can fail to be one: no separator, a
    /// MAC that is not hex, of the wrong length, or simply not this key's over
    /// this value. Telling them apart would be telling whoever sent the cookie
    /// which part of it was wrong.
    ///
    /// `rsplit_once` rather than `split_once`, so a separator appearing inside
    /// a future value cannot silently change which bytes were signed.
    pub fn verify<'a>(&self, cookie: &'a str) -> Option<&'a str> {
        let (value, written) = cookie.rsplit_once(SEPARATOR)?;

        // The length check inside `from_hex` is on the presented MAC and cannot
        // branch on anything secret.
        let mut presented = [0u8; HASH_BYTES];
        if !from_hex(written, &mut presented) {
            return None;
        }

        // `subtle`, never `==`: a `==` over the bytes short-circuits at the
        // first difference, so a valid MAC can be built up byte by byte from
        // enough attempts.
        let expected = self.mac(value.as_bytes());
        bool::from(presented[..].ct_eq(&expected[..])).then_some(value)
    }

    /// HMAC-SHA256 of `message` under this key — RFC 2104.
    ///
    /// The key is exactly [`HASH_BYTES`] and the block is [`BLOCK_BYTES`], so
    /// the two cases RFC 2104 has for a key — hash it if it is longer than a
    /// block, zero-pad it otherwise — collapse to the second.
    fn mac(&self, message: &[u8]) -> [u8; HASH_BYTES] {
        mac_with(&self.0, message)
    }
}

/// Hand-written: no key material in a rendering.
impl fmt::Debug for CookieKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CookieKey(<redacted>)")
    }
}

/// HMAC-SHA256 with a key of at most one block.
///
/// Split out from [`CookieKey::mac`] so the RFC 4231 vectors — whose keys are
/// 4 and 20 bytes — test the construction this server uses rather than a
/// second copy of it written for the test.
///
/// # Panics
///
/// In a debug build, if `key` is longer than one SHA-256 block. RFC 2104 says
/// to hash such a key first; this server has no such key, and a silent
/// truncation would be the wrong way to find out that it had grown one.
fn mac_with(key: &[u8], message: &[u8]) -> [u8; HASH_BYTES] {
    debug_assert!(
        key.len() <= BLOCK_BYTES,
        "a key longer than one block needs RFC 2104's shortening step"
    );

    let mut inner_pad = [0x36u8; BLOCK_BYTES];
    let mut outer_pad = [0x5cu8; BLOCK_BYTES];
    for (index, byte) in key.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);

    outer.finalize().into()
}

/// A fresh opaque identifier: 256 bits of OS entropy as lowercase hex.
///
/// Both a session id and an OAuth `state` are one of these: a token nobody may
/// guess, meaningful only as a key into a server-side record.
///
/// # Panics
///
/// If the OS entropy source cannot produce 32 bytes. There is no weaker
/// fallback to reach for, and a server that cannot obtain entropy must not
/// hand out a session.
pub fn opaque_id() -> String {
    let mut bytes = [0u8; ID_BYTES];

    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("the OS entropy source is unavailable, so no session can be established");

    to_hex(&bytes)
}

/// Fills `bytes` from `written`, and says whether it was hex of that width.
///
/// `false` covers both a wrong length and a character that is not a digit; the
/// caller that has to tell them apart checks the length itself first. `bytes`
/// is left partly written on a refusal, which no caller reads.
fn from_hex(written: &str, bytes: &mut [u8]) -> bool {
    if written.len() != 2 * bytes.len() {
        return false;
    }

    let (pairs, rest) = written.as_bytes().as_chunks::<2>();
    debug_assert!(rest.is_empty(), "the length was just checked to be even");

    for (byte, pair) in bytes.iter_mut().zip(pairs) {
        match (nibble(pair[0]), nibble(pair[1])) {
            (Some(high), Some(low)) => *byte = (high << 4) | low,
            _ => return false,
        }
    }

    true
}

/// One hex digit's value, or [`None`] for a byte that is not one.
///
/// Lowercase only. An uppercase cookie is a cookie this server did not write,
/// and accepting one would make two spellings of the same MAC.
const fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// Renders bytes as lowercase hex.
fn to_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(2 * bytes.len());
    for byte in bytes {
        text.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        text.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key of the right shape, and not one this server would ever generate.
    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn key() -> CookieKey {
        CookieKey::parse(KEY).expect("64 lowercase hex characters")
    }

    #[test]
    fn the_mac_matches_the_published_hmac_sha256_vectors() {
        // RFC 4231's first three test cases. Cases 1 and 3 use a 20-byte key
        // and case 2 a 4-byte one, so all three exercise the zero-padding this
        // server's 32-byte key takes.
        let cases: [(Vec<u8>, Vec<u8>, &str); 3] = [
            (
                vec![0x0b; 20],
                b"Hi There".to_vec(),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?".to_vec(),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                vec![0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
        ];

        for (key, message, expected) in cases {
            assert_eq!(to_hex(&mac_with(&key, &message)), expected);
        }
    }

    /// Why `written` is not a key. `CookieKey` has no `PartialEq`, so a
    /// refusal is read out of the `Err` side rather than by comparing two
    /// `Result`s.
    fn refused(written: &str) -> KeyError {
        match CookieKey::parse(written) {
            Err(error) => error,
            Ok(key) => panic!("{written:?} was accepted as {key:?}"),
        }
    }

    #[test]
    fn a_key_is_sixty_four_lowercase_hex_characters_and_nothing_else() {
        assert!(CookieKey::parse(KEY).is_ok());

        assert_eq!(
            refused(&KEY[..63]),
            KeyError::Length {
                expected: 64,
                got: 63
            }
        );
        assert_eq!(
            refused(&format!("{KEY}0")),
            KeyError::Length {
                expected: 64,
                got: 65
            }
        );
        assert_eq!(
            refused(""),
            KeyError::Length {
                expected: 64,
                got: 0
            }
        );
        // The right length, and not hex.
        assert_eq!(refused(&KEY.replace('a', "z")), KeyError::NotHex);
        // Uppercase is refused too: accepting it would be two spellings of one
        // key.
        assert_eq!(refused(&KEY.to_uppercase()), KeyError::NotHex);
    }

    #[test]
    fn a_signed_value_comes_back_out_of_its_own_cookie() {
        let key = key();
        let value = opaque_id();

        let cookie = key.signed(&value);

        assert!(cookie.starts_with(&value), "{cookie}");
        assert_eq!(cookie.len(), value.len() + 1 + CookieKey::TEXT_LEN);
        assert_eq!(key.verify(&cookie), Some(value.as_str()));
    }

    /// `hex` with one digit changed to a different one. A rotation rather than
    /// a fixed replacement, so it moves the digit whatever it was.
    fn edited(hex: &str) -> String {
        let mut digits: Vec<char> = hex.chars().collect();
        let index = digits.len() / 2;
        digits[index] = match digits[index] {
            'f' => '0',
            '9' => 'a',
            other => char::from_u32(other as u32 + 1).expect("a hex digit has a successor"),
        };

        digits.into_iter().collect()
    }

    #[test]
    fn a_tampered_value_and_a_tampered_mac_are_both_refused() {
        // The two halves a client can edit.
        let key = key();
        let value = opaque_id();
        let cookie = key.signed(&value);

        let (value, mac) = cookie.rsplit_once(SEPARATOR).expect("it was just signed");

        assert_eq!(key.verify(&format!("{}.{mac}", edited(value))), None);
        assert_eq!(key.verify(&format!("{value}.{}", edited(mac))), None);

        // And the untouched one still verifies, so the edits above are what the
        // refusals are about.
        assert!(key.verify(&cookie).is_some());
    }

    #[test]
    fn another_key_does_not_verify_this_keys_cookie() {
        let cookie = key().signed(&opaque_id());
        let other = CookieKey::parse(&"f".repeat(64)).expect("64 lowercase hex characters");

        assert_eq!(other.verify(&cookie), None);
    }

    #[test]
    fn a_cookie_that_is_not_one_is_refused_rather_than_parsed() {
        let key = key();
        let value = opaque_id();

        // No separator at all; a MAC of the wrong length; a MAC that is not hex;
        // an empty MAC. Every one of them is the same answer.
        for cookie in [
            value.clone(),
            format!("{value}.{}", "0".repeat(63)),
            format!("{value}.{}", "z".repeat(64)),
            format!("{value}."),
            String::new(),
            SEPARATOR.to_string(),
        ] {
            assert_eq!(key.verify(&cookie), None, "{cookie}");
        }
    }

    #[test]
    fn the_last_separator_is_what_divides_a_cookie() {
        // A value containing the separator is signed as a whole and read back as
        // a whole, so a future value with a dot in it cannot silently change
        // which bytes were signed.
        let key = key();

        let cookie = key.signed("a.b.c");

        assert_eq!(key.verify(&cookie), Some("a.b.c"));
    }

    #[test]
    fn two_opaque_ids_differ_and_are_sixty_four_lowercase_hex_characters() {
        let first = opaque_id();
        let second = opaque_id();

        assert_ne!(first, second);
        for id in [&first, &second] {
            assert_eq!(id.len(), 64);
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "not lowercase hex: {id}"
            );
        }
    }

    #[test]
    fn debug_prints_a_placeholder_and_no_part_of_the_key() {
        let printed = format!("{:?}", key());

        assert_eq!(printed, "CookieKey(<redacted>)");
        for length in (4..=CookieKey::TEXT_LEN).step_by(4) {
            assert!(!printed.contains(&KEY[..length]));
        }
    }
}
