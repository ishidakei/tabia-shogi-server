//! One finished game, as the `games` table and its sidecar both spell it.
//!
//! This module knows no game rules: deciding a tag from a start specification
//! stays in the session layer, where a game is created.
//!
//! The `games` row and the `.meta` sidecar carry the same fourteen fields
//! under the same fourteen names, so a row can be rebuilt from a sidecar with
//! nothing else in hand. [`GameRow`] is what the insert binds and what the
//! sidecar serializes, so a column added to one is a column added to the
//! other.
//!
//! `token_key` is an identity, not a credential: the hex form of
//! [`auth::token::hash`]'s digest of the token string presented at `LOGIN`, in
//! both authentication modes, so a token issued later matches its own earlier
//! games with no translation table. The token itself is never here.
//!
//! [`auth::token::hash`]: crate::auth::token::hash

use serde::{Deserialize, Serialize};

use crate::auth::TokenHash;

/// Lowercase, so that two spellings of one digest cannot both reach the column.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// The number of characters a `*_token_key` has: SHA-256 is 32 bytes, two hex
/// digits each.
pub const TOKEN_KEY_LEN: usize = 64;

/// The identity a game is filed under, from the hash `auth::token` produced.
///
/// The encoding lives here rather than in `auth`: whether a stored digest is a
/// blob or a hex string is the storage layer's decision.
pub fn token_key(hash: &TokenHash) -> String {
    let mut text = String::with_capacity(TOKEN_KEY_LEN);
    for byte in hash.as_bytes() {
        text.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        text.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    text
}

/// [`token_key`]'s inverse: a stored key read back as the hash it spells, or
/// `None` for text that is not one.
///
/// The length must be exactly [`TOKEN_KEY_LEN`] and every digit must be
/// lowercase hex: `token_key` writes nothing else, so an uppercase digit is a
/// value some other writer produced.
pub fn token_hash(key: &str) -> Option<TokenHash> {
    if key.len() != TOKEN_KEY_LEN {
        return None;
    }

    let mut bytes = [0u8; TOKEN_KEY_LEN / 2];
    let (pairs, _) = key.as_bytes().as_chunks::<2>();
    for (byte, digits) in bytes.iter_mut().zip(pairs) {
        *byte = (nibble(digits[0])? << 4) | nibble(digits[1])?;
    }

    Some(TokenHash::from_bytes(bytes))
}

/// Whether `text` is a participant ID — a value [`token_key`] could have
/// written.
///
/// The shape check for an identifier somebody typed: an administrator
/// designating a rating writes a participant ID into a form, and one that is
/// not a digest would designate nobody in silence.
pub fn is_token_key(text: &str) -> bool {
    token_hash(text).is_some()
}

/// One lowercase hex digit's value, or `None`.
fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// Which kind of starting position a game was played from.
///
/// [`Handicap`](Self::Handicap) is reserved: no game reaches it today, and the
/// column's `CHECK` admits it so that handicap support adds games rather than
/// a migration.
///
/// The three words name the shape of the start and nothing else. In particular
/// [`Designated`](Self::Designated) does not claim the position is even: this
/// server has no evaluation function.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartCategory {
    /// A buoy with an empty setup: play began at hirate.
    Hirate,

    /// A buoy with a setup sequence: a position the operator designated.
    Designated,

    /// A written board. Handicap only, and unreachable today.
    Handicap,
}

impl StartCategory {
    /// The word the column holds.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hirate => "hirate",
            Self::Designated => "designated",
            Self::Handicap => "handicap",
        }
    }
}

/// Whether the two sides began with the same allowance.
///
/// `asymmetric` is exactly "the time configuration states a reduction", which
/// is the only way the two allowances can differ.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeCategory {
    /// Both sides started with the same allowance.
    Symmetric,

    /// One side's initial allowance was reduced.
    Asymmetric,
}

impl TimeCategory {
    /// The word the column holds.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symmetric => "symmetric",
            Self::Asymmetric => "asymmetric",
        }
    }
}

/// Who won.
///
/// Not the protocol's `#WIN` / `#LOSE` / `#DRAW`, which is a per-client line
/// and so says nothing on its own about which engine it went to.
///
/// [`Nobody`](Self::Nobody) is for an outcome with no winner that is not a
/// draw. Exactly one outcome reaches it — the server's own abort, which is
/// evidence about neither engine.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Winner {
    /// Black won.
    Black,

    /// White won.
    White,

    /// A draw: `#SENNICHITE`, `#MAX_MOVES`, or a jishogi draw.
    Draw,

    /// No winner, and not a draw.
    #[serde(rename = "none")]
    Nobody,
}

impl Winner {
    /// The word the column holds.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::White => "white",
            Self::Draw => "draw",
            Self::Nobody => "none",
        }
    }
}

/// One finished game: a `games` row, and a `.meta` sidecar's whole content.
///
/// Owned strings rather than borrows, because both destinations outlive the
/// game task that fills it in.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GameRow {
    /// The CSA `Game_ID`, and the primary key.
    pub game_id: String,

    /// Black's engine name, as given at `LOGIN`.
    pub black_name: String,

    /// White's engine name, as given at `LOGIN`.
    pub white_name: String,

    /// Black's identity: [`token_key`] of the token presented at `LOGIN`.
    pub black_token_key: String,

    /// White's identity.
    pub white_token_key: String,

    /// Which kind of position the game started from.
    pub start_category: StartCategory,

    /// Whether the allowances were symmetric.
    pub time_category: TimeCategory,

    /// When `START` went out, RFC 3339 in UTC.
    pub started_at: String,

    /// When the game ended, RFC 3339 in UTC.
    pub ended_at: String,

    /// The CSA end status word without its `#` — `RESIGN`, `TIME_UP` — with one
    /// exception: a disconnect writes `DISCONNECT`, which is no status word,
    /// because its wire lines are a resignation's and the column has to keep the
    /// two apart.
    pub end_status: String,

    /// Who won.
    pub result: Winner,

    /// Setup moves plus played moves.
    pub ply_count: u32,

    /// The `.csa` file's path, relative to the records directory.
    pub record_path: String,

    /// The canonical USI `position` line this game started from — the identity
    /// the starting-position statistics are grouped by.
    ///
    /// The line itself is the identity, compared in full. Canonical means the
    /// form the collection loader settles on, so one position reaches this
    /// field as the same bytes however the operator wrote it.
    ///
    /// `None` for a row with no stored start position. Such a row is invisible
    /// to the statistics rather than counted under some default, and a sidecar
    /// with no start position parses back with `None` rather than failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_position: Option<String>,
}

/// One participant, as the `games` table alone knows one.
///
/// The identity is the token key: a different token is a different engine.
/// The row is derived rather than stored — there is no `participants` table.
///
/// [`display_name`](Self::display_name) is the engine name that key most
/// recently played under, read off the games rather than off the `tokens`
/// table because that is the one source answering in both authentication
/// modes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticipantRow {
    /// The identity: [`token_key`] of the token presented at `LOGIN`.
    pub token_key: String,

    /// The engine name of the newest game this key played.
    pub display_name: String,

    /// How many finished games it has played, on either side.
    pub games: u64,

    /// When the newest of them ended, RFC 3339 in UTC.
    pub last_ended_at: String,
}

/// One finished game, as a rating publication reads it.
///
/// A narrow row, not a [`GameRow`]: a publication reads two years of history
/// in one query, and eight of the fourteen columns answer every question the
/// fit asks. A job that selected the other six would be carrying the whole
/// history's worth of them through memory to ignore them.
///
/// [`game_id`](Self::game_id) is not a question the fit asks either. It is the
/// tiebreak: two games of one token that ended in the same second decide the
/// display name by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RatingRow {
    /// The CSA `Game_ID` — the tiebreak, and nothing else.
    pub game_id: String,

    /// Black's identity.
    pub black_token_key: String,

    /// White's identity.
    pub white_token_key: String,

    /// Black's engine name, as given at `LOGIN`.
    pub black_name: String,

    /// White's engine name.
    pub white_name: String,

    /// Who won.
    pub result: Winner,

    /// The end status word, which the short-disconnect exclusion reads
    /// `DISCONNECT` off.
    pub end_status: String,

    /// Setup moves plus played moves — not the played count that exclusion
    /// measures.
    pub ply_count: u32,

    /// The canonical USI `position` line, whose move list is the setup sequence
    /// that subtraction uses. `None` for a row with no stored start position,
    /// whose setup length is therefore unknowable.
    pub start_position: Option<String>,

    /// When the game ended, RFC 3339 in UTC — what the age decay measures from.
    pub ended_at: String,
}

/// What the `games` table knows about one starting position.
///
/// The finished half of the UCB rule's input: `n`'s recorded component and the
/// three decided outcomes, counted from Black's side because that is the side
/// the win rate is measured on. The in-flight component of `n` is not here —
/// no row exists for a game still being played.
///
/// A `none` result is in [`games`](Self::games) and in none of the other
/// three: it is a game started from the position, and says nothing about which
/// side the position favors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PositionOutcomes {
    /// Finished games recorded from this position, whatever their result.
    pub games: u64,

    /// How many of them Black won.
    pub black_wins: u64,

    /// How many of them White won.
    pub white_wins: u64,

    /// How many of them were drawn.
    pub drawn: u64,
}

/// A filled-in row, shared by the tests of this module and of the two that
/// write it, so a field cannot be dropped on one path and asserted on another.
#[cfg(test)]
pub(super) fn sample_row(game_id: &str) -> GameRow {
    GameRow {
        game_id: game_id.to_owned(),
        black_name: "engine-a".to_owned(),
        white_name: "engine-b".to_owned(),
        black_token_key: token_key(&crate::auth::token::hash("token-for-engine-a")),
        white_token_key: token_key(&crate::auth::token::hash("token-for-engine-b")),
        start_category: StartCategory::Hirate,
        time_category: TimeCategory::Symmetric,
        started_at: "2026-08-19T12:00:00Z".to_owned(),
        ended_at: "2026-08-19T12:04:00Z".to_owned(),
        end_status: "RESIGN".to_owned(),
        result: Winner::White,
        ply_count: 41,
        record_path: format!("{game_id}.csa"),
        start_position: Some("position startpos moves 7g7f 3c3d".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::auth::token;

    #[test]
    fn a_token_key_is_the_hex_of_the_hash_auth_produces() {
        // FIPS 180-4's SHA-256 of "abc".
        assert_eq!(
            token_key(&token::hash("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_token_key_is_not_the_token_and_has_sha256s_length() {
        let presented = "token-for-engine-a";
        let key = token_key(&token::hash(presented));

        assert_eq!(key.len(), TOKEN_KEY_LEN);
        assert!(!key.contains(presented));
        assert!(
            key.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{key}"
        );
    }

    #[test]
    fn a_key_decodes_back_to_the_hash_it_was_written_from() {
        // A row read back is the digest the issuing page handed to storage.
        for presented in ["abc", "token-for-engine-a", ""] {
            let hash = token::hash(presented);

            assert_eq!(token_hash(&token_key(&hash)), Some(hash), "{presented}");
        }
    }

    #[test]
    fn text_that_is_not_a_key_decodes_to_nothing() {
        let key = token_key(&token::hash("abc"));

        // Too short, too long, a non-hex digit, and an uppercase one, which
        // `token_key` never writes.
        assert_eq!(token_hash(&key[..TOKEN_KEY_LEN - 1]), None);
        assert_eq!(token_hash(&format!("{key}0")), None);
        assert_eq!(
            token_hash(&format!("{}zz", &key[..TOKEN_KEY_LEN - 2])),
            None
        );
        assert_eq!(token_hash(&key.to_uppercase()), None);
        assert_eq!(token_hash(""), None);
    }

    #[test]
    fn one_token_yields_one_key() {
        assert_eq!(
            token_key(&token::hash("token-for-engine-a")),
            token_key(&token::hash("token-for-engine-a"))
        );
        assert_ne!(
            token_key(&token::hash("token-for-engine-a")),
            token_key(&token::hash("token-for-engine-b"))
        );
    }

    #[test]
    fn every_tag_spells_itself_the_way_the_check_constraint_spells_it() {
        // The three lists in the migration's `CHECK` clauses, written out: a
        // variant renamed on one side and not the other is an insert the
        // database refuses at runtime.
        assert_eq!(StartCategory::Hirate.as_str(), "hirate");
        assert_eq!(StartCategory::Designated.as_str(), "designated");
        assert_eq!(StartCategory::Handicap.as_str(), "handicap");
        assert_eq!(TimeCategory::Symmetric.as_str(), "symmetric");
        assert_eq!(TimeCategory::Asymmetric.as_str(), "asymmetric");
        assert_eq!(Winner::Black.as_str(), "black");
        assert_eq!(Winner::White.as_str(), "white");
        assert_eq!(Winner::Draw.as_str(), "draw");
        assert_eq!(Winner::Nobody.as_str(), "none");
    }

    #[test]
    fn a_tag_serializes_as_the_word_it_spells() {
        // The sidecar's words and the column's words are the same words.
        let toml = toml::to_string(&sample_row("20260819-tabia-1-0")).expect("the row serializes");

        assert!(toml.contains("start_category = \"hirate\""), "{toml}");
        assert!(toml.contains("time_category = \"symmetric\""), "{toml}");
        assert!(toml.contains("result = \"white\""), "{toml}");
    }
}
