//! Client lines as typed commands.
//!
//! One framed line in, one classification out: a command the session can act
//! on, a `LOGIN` that failed, or a line this server does not recognize. The
//! three are answered differently — the session acts on the first, the second
//! gets `LOGIN:incorrect`, and the third gets silence with a log entry — so
//! producing the distinction is this layer's whole job. Producing the replies is
//! the session's.
//!
//! [`split_comment`] runs before any of it, because a comment is not a
//! classification: it is text this server does not read, and removing it is
//! what leaves a line to classify.
//!
//! Nothing here reads or writes; the codec above it has already bounded the
//! line's length and validated its encoding.

use super::codec::MAX_LINE_LEN;

/// Longest engine name accepted in a `LOGIN`, in characters.
///
/// shogi-server's identifier set, at length 1–1024. Its own default of 32 is
/// too tight for real engine
/// names and it treats the limit as configurable (`--max-identifier`), so the
/// wider bound stays inside compatibility — **shogi-server-compatible**.
pub const MAX_ENGINE_NAME_LEN: usize = 1024;

/// Longest token accepted in a `LOGIN`, in characters.
///
/// Printable ASCII excluding space, at length 1–64, compared byte for byte.
pub const MAX_TOKEN_LEN: usize = 64;

/// The field bounds and the codec's line cap are one decision recorded twice:
/// `MAX_LINE_LEN` exists to clear a maximal `LOGIN` line, and these constants
/// are what "maximal" means. Raising either past what the codec will carry
/// would make a valid login unreachable — the codec would refuse the line
/// before this parser ever saw it — so it is a build error rather than a
/// runtime mystery. The assertion sits here, not in `codec.rs`, so the
/// dependency runs the way the layers do: commands know about framing,
/// framing knows nothing about commands.
const _: () = assert!(
    MAX_LINE_LEN >= "LOGIN ".len() + MAX_ENGINE_NAME_LEN + " ".len() + MAX_TOKEN_LEN,
    "the line cap must clear a maximal LOGIN line"
);

/// The character that starts a comment anywhere in a client line.
///
/// One byte in UTF-8, which is why the split below can index at the byte offset
/// [`str::find`] returns without checking for a character boundary.
const COMMENT: char = '\'';

/// One client line, split at its comment.
///
/// `Debug` is derived, and it can print a token: a `LOGIN` line reaches
/// [`split_comment`] before anything has decided it is a login, so no field here
/// is *known* to hold credential material. That is the same position
/// [`Unparsed::Unknown`] records, and for the same reason — invariant 8 governs
/// fields known to carry a token, and the knowledge lives in the login path
/// below, not in a split that runs ahead of every classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Commented<'a> {
    /// What is left to parse: everything before the first `'`, with one
    /// trailing `,` removed. The whole line when it carried no comment.
    pub command: &'a str,

    /// The comment, from its `'` to the end of the line, or `None` when there
    /// was none. Carried for a log record and nothing else — this server does
    /// not store it, and never relays it.
    pub comment: Option<&'a str>,
}

/// Splits one client line into the part to parse and the comment.
///
/// **A `'` anywhere in a client line starts a comment.** Not specification
/// text: the CSA server protocol v1.2.1 has no comment syntax. It is the
/// floodgate convention, and the clients this server exists for send it —
/// shogi-server's own `bin/usiToCsa.rb` appends an evaluation and a principal
/// variation to *every* move once its engine reports one, with no option to
/// turn it off:
///
/// ```text
/// +2726FU,'* 56 -8384FU +2625FU -8485FU
/// ```
///
/// and shogi-server accepts it (`shogi_server/command.rb`, `MoveCommand#call`:
/// the line is split on `,` and a field starting with `'` is recorded as a
/// comment). A server that classed the line as malformed would drop every move
/// such a client sends and lose it the game on time, which is exactly what was
/// measured at the M2 gate on 2026-08-16.
///
/// The rule is the whole line's, not the move line's: `%TORYO,'* bye` is a
/// resignation, and a `LOGIN` is split like anything else — so an `open`-mode
/// token cannot contain a `'`.
///
/// **One trailing `,` is removed with the comment**, and only that one: it is
/// the separator shogi-server's form puts between the move and the comment.
/// Nothing else about the remaining text is repaired, so `+7776FU '* 30` —
/// a space where the comma should be — leaves `+7776FU ` and stays the
/// malformed line shogi-server also made it (its changelog, 2020-12-06: "Make
/// invalid comments illegal").
///
/// ```
/// # use tabia_shogi_server::csa::{Commented, split_comment};
/// assert_eq!(
///     split_comment("+2726FU,'* 56 -8384FU"),
///     Commented { command: "+2726FU", comment: Some("'* 56 -8384FU") },
/// );
/// assert_eq!(
///     split_comment("+7776FU"),
///     Commented { command: "+7776FU", comment: None },
/// );
/// ```
pub fn split_comment(line: &str) -> Commented<'_> {
    let Some(at) = line.find(COMMENT) else {
        return Commented {
            command: line,
            comment: None,
        };
    };

    let (command, comment) = line.split_at(at);

    Commented {
        command: command.strip_suffix(',').unwrap_or(command),
        comment: Some(comment),
    }
}

/// A client command, with every payload borrowed from the line it was parsed
/// from.
///
/// The borrow is the codec's buffer-reuse contract made visible: a command
/// must be consumed before the next `read_line()`. A session that keeps the
/// engine name past login owns it there, which is one copy per login rather
/// than one per line.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    /// `LOGIN <username> <password>` — both fields validated per Q4 and Q5.
    /// `name` is the engine name and `token` a tabia token: a use of the
    /// specification's fields, not a change to its syntax (P-1).
    Login {
        /// The engine name, matching `[A-Za-z0-9_@\-\.]{1,1024}`.
        name: &'a str,
        /// The presented token. Whether it is a real one is `auth`'s question.
        token: &'a str,
    },

    /// `LOGOUT`.
    Logout,

    /// `AGREE [<GameID>]`. The Game_ID is opaque here; whether it names the
    /// offered game is session logic (P-3).
    Agree { game_id: Option<&'a str> },

    /// `REJECT [<GameID>]`, on the same terms as [`Command::Agree`].
    Reject { game_id: Option<&'a str> },

    /// A board move, carried raw — `+7776FU` from Black, `-3334FU` from White.
    ///
    /// The client sends the bare move; the `,T` suffix is what the *relay*
    /// appends (P-4), so `+7776FU,T12` is [`super::Response::Move`]'s output
    /// rather than this side's input.
    ///
    /// Classification is by the leading `+` or `-` alone. Notation is
    /// `csa/notation.rs`'s territory and needs `game::Move`, so a line that
    /// merely starts like a move (a bare `+`) is a move to this layer and is
    /// rejected by that one.
    Move { line: &'a str },

    /// `%TORYO` — resignation (P-7, `#RESIGN`).
    Resign,

    /// `%KACHI` — a jishogi declaration (P-7, `#JISHOGI` if valid,
    /// `#ILLEGAL_MOVE` if not).
    DeclareWin,

    /// `%CHUDAN` — a suspension request (P-7). Recognized here because clients
    /// send it; suspension itself is not supported, and the session layer
    /// adjudicates the line as an illegal move by its sender.
    Suspend,

    /// A shogi-server extension command: any `%%` line, `%%GAME` and
    /// `%%SETBUOY` and `%%WHO` alike. Recognized, never implemented — Q7
    /// answers it `##[WARN] unknown command: <command>` and keeps the
    /// connection open, echoing the line carried here.
    Extension { line: &'a str },

    /// A keep-alive: the empty line, or a line holding exactly one space.
    ///
    /// An application-level protocol shogi-server names in `command.rb`, above
    /// its `SpecialCommand`:
    ///
    /// ```text
    /// # Keep Alive is an application-level protocol here. There are two representations:
    /// # 1) LF (empty string)
    /// #    The server sends back an LF (empty string).
    /// #    Note that the 30 sec rule (client may not send LF again within 30 sec)
    /// #    is not implemented yet.
    /// #    This is compliant with CSA's protocol in certain situations.
    /// # 2) Space + LF (a single space)
    /// #    The sever replies nothing.
    /// #    This is an enhancement to CSA's protocol.
    /// ```
    ///
    /// The two differ in the reply and in nothing else, which is why `echo` is a
    /// field rather than a second variant: the side effect the session runs for
    /// one it runs for the other, and a variant apiece would be an invitation to
    /// state that twice.
    ///
    /// **The 30-second rule is not implemented**, as in the reference. A
    /// keep-alive is never counted as a malformed line, so a client that sends
    /// them faster than the rule would allow is not disconnected for it.
    KeepAlive {
        /// Whether this one is owed an empty line back — `true` for `""`,
        /// `false` for `" "`.
        echo: bool,
    },

    /// A line that is whitespace and nothing else, longer than the single space
    /// above: shogi-server's `SpaceCommand`, which it documents as "ignored, no
    /// reply".
    ///
    /// Not a keep-alive. It earns no reply *and* no side effect, where a
    /// keep-alive earns the deadline check its state has — which is the whole
    /// difference between `Command.factory`'s `when "", " "` arm and its
    /// `when /^\s*$/` one.
    Whitespace,
}

/// Whether `line` is whitespace and nothing else, on Ruby's reading of `\s`.
///
/// ASCII-only, deliberately. `/^\s*$/` in `Command.factory` matches
/// `[ \t\r\n\f\v]` and no more, so a line of ideographic spaces is not blank to
/// the reference — and [`char::is_whitespace`], which is Unicode-aware, would
/// have made it blank here. The codec admits any UTF-8, so the difference is
/// reachable rather than theoretical.
///
/// `\r` and `\n` can only appear as content: the framing layer has already taken
/// the terminator off.
fn is_blank(line: &str) -> bool {
    line.chars()
        .all(|c| matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{b}' | '\u{c}'))
}

/// Hand-written because [`Command::Login`] holds a token and a derived `Debug`
/// would print it (invariant 8: no credential material in a rendering). Matching
/// over the variants rather than redacting inside a newtype is what makes the
/// guarantee exhaustive: a variant added later must be written in here, so it
/// cannot inherit a leak by default.
impl std::fmt::Debug for Command<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Login { name, .. } => f
                .debug_struct("Login")
                .field("name", name)
                .field("token", &"<redacted>")
                .finish(),
            Self::Logout => f.write_str("Logout"),
            Self::Agree { game_id } => f.debug_struct("Agree").field("game_id", game_id).finish(),
            Self::Reject { game_id } => f.debug_struct("Reject").field("game_id", game_id).finish(),
            Self::Move { line } => f.debug_struct("Move").field("line", line).finish(),
            Self::Resign => f.write_str("Resign"),
            Self::DeclareWin => f.write_str("DeclareWin"),
            Self::Suspend => f.write_str("Suspend"),
            Self::Extension { line } => f.debug_struct("Extension").field("line", line).finish(),
            Self::KeepAlive { echo } => f.debug_struct("KeepAlive").field("echo", echo).finish(),
            Self::Whitespace => f.write_str("Whitespace"),
        }
    }
}

/// Why a line is not a command, in the two shapes the session must tell apart.
///
/// This is not a [`super::Error`]: a line that does not parse is one of this
/// layer's normal answers, not a failure of it, and unlike a framing error it
/// is not fatal to the connection. Part 5 closes only on *repeated*
/// occurrences, which is a count the session keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unparsed<'a> {
    /// A `LOGIN` line that is not a valid login. The session answers
    /// `LOGIN:incorrect` (P-1) — which is why this is not merged into
    /// [`Unparsed::Unknown`], whose answer is nothing at all.
    Login(LoginRejection<'a>),

    /// Any other unrecognized line, carried as received so Part 5 can log what
    /// actually arrived.
    ///
    /// This is the one place a mistyped credential can still reach a log: a
    /// client that sends `login <name> <token>` in lowercase produces an
    /// unknown line containing its token. Nothing here can tell that apart
    /// from junk, and Part 5 needs the line to reconstruct what happened —
    /// invariant 8 governs fields *known* to hold token material, and the
    /// login path above is where that knowledge lives.
    Unknown(&'a str),
}

/// Why a `LOGIN` failed, carrying no token text in any variant.
///
/// `Debug` is derived, which is only safe because of that: the engine name is
/// carried by exactly one variant, and only where it is known to be a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginRejection<'a> {
    /// Not exactly three fields separated by single spaces — a bare `LOGIN`, a
    /// missing token, a doubled space, or shogi-server's
    /// `LOGIN <name> <password> x1`. tabia implements no x1, so the trailing
    /// field is a failed login rather than a mode switch: a deliberate
    /// non-implementation, not a divergence in shared behavior.
    ///
    /// Nothing is carried. With the arity wrong, the field in the name
    /// position is whatever the client typed — `LOGIN s3cret` puts a token
    /// there — so there is no field here known not to be a credential.
    Arity,

    /// The engine name failed Q4's charset or length. The offending text is
    /// not carried, for the same reason.
    Name,

    /// The engine name is well formed and the token is not (Q5).
    ///
    /// This is the only variant that carries a name, and the only case that
    /// can: three fields were present and the second validated as a name, so
    /// the third is known to be the token and the second known not to be.
    Token { name: &'a str },
}

impl<'a> Command<'a> {
    /// Classifies one framed line.
    ///
    /// The line arrives from [`super::LineReader`] with its terminator
    /// stripped, its length bounded, and its encoding checked, so the only
    /// question left is what it says. Commands are case-sensitive and
    /// uppercase, per the specification.
    ///
    /// | Line | Classification | Answer owed |
    /// |---|---|---|
    /// | `+7776FU`, `-3334FU` | [`Move`](Self::Move) | the relay, once the game has read it |
    /// | `%TORYO` / `%KACHI` / `%CHUDAN` | [`Resign`](Self::Resign) / [`DeclareWin`](Self::DeclareWin) / [`Suspend`](Self::Suspend) | a termination |
    /// | `%%…` | [`Extension`](Self::Extension) | `##[WARN] unknown command: <line>` |
    /// | `LOGIN <name> <token>` | [`Login`](Self::Login) | `LOGIN:<name> OK` or `LOGIN:incorrect` |
    /// | `LOGOUT` | [`Logout`](Self::Logout) | `LOGOUT:completed` |
    /// | `AGREE` / `REJECT`, with an optional id | [`Agree`](Self::Agree) / [`Reject`](Self::Reject) | the pairing's |
    /// | *(an empty line)* | [`KeepAlive { echo: true }`](Self::KeepAlive) | one empty line |
    /// | `" "` (one space) | [`KeepAlive { echo: false }`](Self::KeepAlive) | nothing |
    /// | whitespace only, longer | [`Whitespace`](Self::Whitespace) | nothing |
    /// | anything else | [`Unparsed`] | nothing, or `LOGIN:incorrect` |
    ///
    /// **The empty line is a command, not a malformed line** (decided
    /// 2026-08-17). `Command.factory` in shogi-server's
    /// `command.rb` classifies `""` and `" "` before it consults the player's
    /// status, and `SpecialCommand#call` answers the first with an LF and the
    /// second with nothing; a longer whitespace-only line becomes its
    /// `SpaceCommand` and is ignored. A server that counted the empty line
    /// toward `[server].max_malformed_lines` would disconnect a client using it
    /// as a keep-alive — **shogi-server-compatible**, and the state-independence
    /// is the reference's own ordering rather than a simplification of it.
    pub fn parse(line: &'a str) -> Result<Self, Unparsed<'a>> {
        // The keep-alive forms, ahead of everything: they are exact literals,
        // and an empty line matches no prefix test below anyway.
        match line {
            "" => return Ok(Self::KeepAlive { echo: true }),
            " " => return Ok(Self::KeepAlive { echo: false }),
            // Longer, and still nothing but whitespace: ignored outright.
            blank if is_blank(blank) => return Ok(Self::Whitespace),
            _ => {}
        }

        // Prefix forms next. A move is classified by its leading sign alone,
        // and every `%%` line is an extension command whatever follows.
        if line.starts_with('+') || line.starts_with('-') {
            return Ok(Self::Move { line });
        }
        if line.starts_with("%%") {
            return Ok(Self::Extension { line });
        }

        // Then the forms that take no argument. These are exact matches, so
        // ordering them after the `%%` test is readability, not necessity.
        match line {
            "%TORYO" => return Ok(Self::Resign),
            "%KACHI" => return Ok(Self::DeclareWin),
            "%CHUDAN" => return Ok(Self::Suspend),
            "LOGOUT" => return Ok(Self::Logout),
            _ => {}
        }

        let (keyword, argument) = match line.split_once(' ') {
            Some((keyword, argument)) => (keyword, Some(argument)),
            None => (line, None),
        };

        match keyword {
            "LOGIN" => Self::parse_login(line),
            "AGREE" | "REJECT" => {
                let game_id = match argument {
                    None => None,
                    // One argument, or it is not an `AGREE` line at all. Only
                    // `LOGIN` has an answer of its own for a malformed line;
                    // everything else falls to Part 5's silence.
                    Some(id) if !id.is_empty() && !id.contains(' ') => Some(id),
                    Some(_) => return Err(Unparsed::Unknown(line)),
                };
                Ok(if keyword == "AGREE" {
                    Self::Agree { game_id }
                } else {
                    Self::Reject { game_id }
                })
            }
            _ => Err(Unparsed::Unknown(line)),
        }
    }

    /// Validates `LOGIN <username> <password>` per Q4 and Q5.
    ///
    /// Every failure here is a failed login rather than an unknown line: the
    /// session owes it `LOGIN:incorrect` (P-1).
    fn parse_login(line: &'a str) -> Result<Self, Unparsed<'a>> {
        // `split(' ')`, never `split_whitespace`: the specification separates
        // fields with single spaces, and collapsing runs would silently accept
        // `LOGIN  name  token`. Taking a fourth field and requiring it absent
        // is what rejects the `x1` form.
        let mut fields = line.split(' ');
        let (Some("LOGIN"), Some(name), Some(token), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(Unparsed::Login(LoginRejection::Arity));
        };

        if !is_engine_name(name) {
            return Err(Unparsed::Login(LoginRejection::Name));
        }
        if !is_token(token) {
            return Err(Unparsed::Login(LoginRejection::Token { name }));
        }

        Ok(Self::Login { name, token })
    }
}

/// Q4: `[A-Za-z0-9_@\-\.]`, 1–1024 characters.
///
/// The length is counted in characters by the PRD. Every character the charset
/// admits is one byte, so `len()` is that count; a multi-byte string fails the
/// charset test under either reading.
fn is_engine_name(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= MAX_ENGINE_NAME_LEN
        && field
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '-' | '.'))
}

/// Q5: printable ASCII excluding space — `0x21..=0x7E` — 1–64 characters.
///
/// A space cannot appear anyway, having already served as the field separator;
/// the charset excludes it so the rule reads as the PRD states it rather than
/// relying on the split.
fn is_token(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= MAX_TOKEN_LEN
        && field.chars().all(|c| matches!(c, '!'..='~'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A maximal name and token, reused by the length tests that bracket them.
    fn maximal_name() -> String {
        "a".repeat(MAX_ENGINE_NAME_LEN)
    }

    fn maximal_token() -> String {
        "b".repeat(MAX_TOKEN_LEN)
    }

    #[test]
    fn parses_a_login() {
        assert_eq!(
            Command::parse("LOGIN engine-1 s3cret-token"),
            Ok(Command::Login {
                name: "engine-1",
                token: "s3cret-token",
            })
        );
    }

    #[test]
    fn parses_a_maximal_login_with_both_fields_intact() {
        let name = maximal_name();
        let token = maximal_token();
        let line = format!("LOGIN {name} {token}");

        assert_eq!(
            Command::parse(&line),
            Ok(Command::Login {
                name: &name,
                token: &token,
            })
        );
    }

    #[test]
    fn accepts_every_character_class_the_engine_name_charset_allows() {
        let name = "AZaz09_@-.";
        let line = format!("LOGIN {name} t");

        assert_eq!(
            Command::parse(&line),
            Ok(Command::Login { name, token: "t" })
        );
    }

    #[test]
    fn accepts_a_token_at_both_ends_of_printable_ascii() {
        // 0x21 and 0x7E, the bounds Q5 draws.
        let token = "!~";
        let line = format!("LOGIN engine-1 {token}");

        assert_eq!(
            Command::parse(&line),
            Ok(Command::Login {
                name: "engine-1",
                token,
            })
        );
    }

    #[test]
    fn rejects_a_login_whose_name_is_one_character_over() {
        let line = format!("LOGIN {} b", "a".repeat(MAX_ENGINE_NAME_LEN + 1));

        assert_eq!(
            Command::parse(&line),
            Err(Unparsed::Login(LoginRejection::Name))
        );
    }

    #[test]
    fn rejects_a_login_whose_token_is_one_character_over() {
        let line = format!("LOGIN engine-1 {}", "b".repeat(MAX_TOKEN_LEN + 1));

        assert_eq!(
            Command::parse(&line),
            Err(Unparsed::Login(LoginRejection::Token { name: "engine-1" }))
        );
    }

    #[test]
    fn rejects_a_login_with_an_empty_field() {
        // Empty fields keep the arity at three, so each field's own length
        // bound is what has to catch them.
        assert_eq!(
            Command::parse("LOGIN  token"),
            Err(Unparsed::Login(LoginRejection::Name))
        );
        assert_eq!(
            Command::parse("LOGIN engine-1 "),
            Err(Unparsed::Login(LoginRejection::Token { name: "engine-1" }))
        );
    }

    #[test]
    fn rejects_a_name_outside_the_charset() {
        for line in ["LOGIN engine/1 token", "LOGIN エンジン token"] {
            assert_eq!(
                Command::parse(line),
                Err(Unparsed::Login(LoginRejection::Name)),
                "accepted {line}"
            );
        }
    }

    #[test]
    fn rejects_a_token_outside_the_charset() {
        for line in ["LOGIN engine-1 to\tken", "LOGIN engine-1 トークン"] {
            assert_eq!(
                Command::parse(line),
                Err(Unparsed::Login(LoginRejection::Token { name: "engine-1" })),
                "accepted {line}"
            );
        }
    }

    #[test]
    fn rejects_a_login_of_the_wrong_arity() {
        for line in [
            "LOGIN",
            "LOGIN engine-1",
            // A doubled space is a fourth field, not a wider separator. This
            // is the test that distinguishes split(' ') from split_whitespace.
            "LOGIN  engine-1 token",
            "LOGIN engine-1 token extra",
        ] {
            assert_eq!(
                Command::parse(line),
                Err(Unparsed::Login(LoginRejection::Arity)),
                "accepted {line}"
            );
        }
    }

    #[test]
    fn rejects_the_x1_extension_form_as_a_failed_login() {
        // shogi-server reads the trailing field as an extension switch. tabia
        // implements no x1, so this is a failed login, not a mode change.
        assert_eq!(
            Command::parse("LOGIN engine-1 s3cret-token x1"),
            Err(Unparsed::Login(LoginRejection::Arity))
        );
    }

    #[test]
    fn a_failed_login_is_not_an_unknown_line() {
        assert!(matches!(
            Command::parse("LOGIN engine-1"),
            Err(Unparsed::Login(_))
        ));
        assert!(matches!(
            Command::parse("login engine-1 token"),
            Err(Unparsed::Unknown(_))
        ));
    }

    #[test]
    fn a_rejection_carries_no_token_text() {
        // The arity case is the one that matters: with two fields, the token
        // sits where a name would, so a rejection that echoed "the name" would
        // echo the credential.
        let rejection = Command::parse("LOGIN s3cret-token").unwrap_err();
        assert!(
            !format!("{rejection:?}").contains("s3cret-token"),
            "rejection leaked a token: {rejection:?}"
        );

        let line = format!("LOGIN engine-1 {}", "s3cret".repeat(20));
        let rejection = Command::parse(&line).unwrap_err();
        assert!(
            !format!("{rejection:?}").contains("s3cret"),
            "rejection leaked a token: {rejection:?}"
        );
    }

    #[test]
    fn debug_omits_the_token_and_keeps_the_name() {
        let command = Command::parse("LOGIN engine-1 s3cret-token").unwrap();

        let debug = format!("{command:?}");
        assert!(!debug.contains("s3cret-token"), "Debug leaked a token");
        assert!(
            debug.contains("engine-1"),
            "Debug dropped the engine name a rejection log needs: {debug}"
        );
    }

    #[test]
    fn parses_logout() {
        assert_eq!(Command::parse("LOGOUT"), Ok(Command::Logout));
    }

    #[test]
    fn parses_bare_agreement_commands() {
        assert_eq!(
            Command::parse("AGREE"),
            Ok(Command::Agree { game_id: None })
        );
        assert_eq!(
            Command::parse("REJECT"),
            Ok(Command::Reject { game_id: None })
        );
    }

    #[test]
    fn round_trips_a_game_id_as_given() {
        let game_id = "20260813-engine1-engine2-1";

        assert_eq!(
            Command::parse(&format!("AGREE {game_id}")),
            Ok(Command::Agree {
                game_id: Some(game_id)
            })
        );
        assert_eq!(
            Command::parse(&format!("REJECT {game_id}")),
            Ok(Command::Reject {
                game_id: Some(game_id)
            })
        );
    }

    #[test]
    fn treats_a_malformed_agreement_command_as_unknown() {
        // Not a failed login: LOGIN:incorrect has no counterpart here, so
        // Part 5's "logged, no reply" is what is left.
        for line in ["AGREE ", "AGREE a b", "REJECT a b"] {
            assert_eq!(Command::parse(line), Err(Unparsed::Unknown(line)));
        }
    }

    #[test]
    fn parses_the_three_special_moves() {
        assert_eq!(Command::parse("%TORYO"), Ok(Command::Resign));
        assert_eq!(Command::parse("%KACHI"), Ok(Command::DeclareWin));
        assert_eq!(Command::parse("%CHUDAN"), Ok(Command::Suspend));
    }

    #[test]
    fn classifies_every_double_percent_line_as_an_extension_command() {
        for line in ["%%WHO", "%%GAME", "%%LIST", "%%SETBUOY buoy_x 1", "%%"] {
            assert_eq!(Command::parse(line), Ok(Command::Extension { line }));
        }
    }

    #[test]
    fn classifies_board_moves_carrying_the_raw_text() {
        for line in ["+7776FU", "-3334FU", "+2726FU,T12"] {
            assert_eq!(Command::parse(line), Ok(Command::Move { line }));
        }
    }

    /// The line the M2 gate actually received, from shogi-server's own bridge.
    const FLOODGATE: &str = "+2726FU,'* 56 -8384FU +2625FU -8485FU";

    #[test]
    fn a_line_with_no_quote_is_returned_whole() {
        for line in ["+7776FU", "%TORYO", "LOGIN engine-1 token", "", "junk,"] {
            assert_eq!(
                split_comment(line),
                Commented {
                    command: line,
                    comment: None,
                },
                "{line} was altered"
            );
        }
    }

    #[test]
    fn the_floodgate_suffix_leaves_the_bare_move() {
        assert_eq!(
            split_comment(FLOODGATE),
            Commented {
                command: "+2726FU",
                comment: Some("'* 56 -8384FU +2625FU -8485FU"),
            }
        );
        assert_eq!(
            Command::parse(split_comment(FLOODGATE).command),
            Ok(Command::Move { line: "+2726FU" })
        );
    }

    #[test]
    fn every_client_line_is_split_the_same_way_not_only_a_move() {
        assert_eq!(
            Command::parse(split_comment("%TORYO,'* bye").command),
            Ok(Command::Resign)
        );
        assert_eq!(
            Command::parse(split_comment("%KACHI,'* 27 points").command),
            Ok(Command::DeclareWin)
        );
        assert_eq!(
            Command::parse(split_comment("AGREE 20260816-tabia-1-0,'* ok").command),
            Ok(Command::Agree {
                game_id: Some("20260816-tabia-1-0"),
            })
        );
    }

    #[test]
    fn a_login_is_split_like_any_other_line_so_a_token_cannot_hold_a_quote() {
        // The documented consequence: an `open`-mode token containing a `'` is
        // truncated at it, and what remains is the token presented.
        assert_eq!(
            Command::parse(split_comment("LOGIN engine-1 tok'en").command),
            Ok(Command::Login {
                name: "engine-1",
                token: "tok",
            })
        );
    }

    #[test]
    fn exactly_one_trailing_comma_goes_with_the_comment() {
        // The separator the shogi-server form uses, and only it: a second comma
        // is content the line keeps, and so fails to parse as it would have.
        assert_eq!(split_comment("+7776FU,'x").command, "+7776FU");
        assert_eq!(split_comment("+7776FU,,'x").command, "+7776FU,");
    }

    #[test]
    fn a_space_before_the_comment_leaves_a_line_that_still_does_not_parse() {
        // shogi-server made this form illegal deliberately (changelog,
        // 2020-12-06), and the trailing space is what keeps it that way here.
        let command = split_comment("+7776FU '* 30").command;

        assert_eq!(command, "+7776FU ");
        assert_eq!(
            crate::csa::WrittenMove::parse(command),
            Err(crate::csa::ParseError::Trailing)
        );
    }

    #[test]
    fn a_line_that_is_only_a_comment_leaves_an_empty_line() {
        // Whatever the empty line does, this does: the split adds no rule of
        // its own for a line that had nothing in front of its comment. Since
        // #87 that is the keep-alive, which is asserted here rather than left
        // to the equality alone — the equality would go on holding if both
        // sides became junk again.
        for line in ["'* hello", "'", ",'* hello"] {
            let split = split_comment(line);

            assert_eq!(split.command, "", "{line}");
            assert_eq!(Command::parse(split.command), Command::parse(""), "{line}");
            assert_eq!(
                Command::parse(split.command),
                Ok(Command::KeepAlive { echo: true }),
                "{line}"
            );
        }
    }

    #[test]
    fn the_two_keep_alive_forms_differ_only_in_the_reply_they_are_owed() {
        assert_eq!(Command::parse(""), Ok(Command::KeepAlive { echo: true }));
        assert_eq!(Command::parse(" "), Ok(Command::KeepAlive { echo: false }));
    }

    #[test]
    fn a_longer_whitespace_only_line_is_ignored_rather_than_kept_alive() {
        // shogi-server's `SpaceCommand`: no reply, and none of the keep-alive's
        // side effects either. Every literal here is one the codec can actually
        // produce — a trailing CR goes with the terminator, so `"\r "` is a line
        // and `"\r"` is not.
        for line in ["  ", "\t", " \t ", "\t\t", "\r ", "\u{b}", "\u{c}"] {
            assert_eq!(Command::parse(line), Ok(Command::Whitespace), "{line:?}");
        }
    }

    #[test]
    fn a_non_ascii_space_is_not_a_blank_line() {
        // Ruby's `\s` is ASCII-only, so the reference makes these unknown lines
        // and so does this. U+3000 IDEOGRAPHIC SPACE and U+00A0 NO-BREAK SPACE.
        for line in ["\u{3000}", "\u{a0}", " \u{3000} "] {
            assert_eq!(
                Command::parse(line),
                Err(Unparsed::Unknown(line)),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_line_that_merely_starts_with_a_space_is_not_blank() {
        // The blank test runs first, so a line it does not match must still
        // reach the ordinary classification.
        assert_eq!(Command::parse(" LOGOUT"), Err(Unparsed::Unknown(" LOGOUT")));
    }

    #[test]
    fn the_comment_runs_from_the_first_quote_to_the_end_of_the_line() {
        assert_eq!(
            split_comment("+7776FU,'a 'b 'c"),
            Commented {
                command: "+7776FU",
                comment: Some("'a 'b 'c"),
            }
        );
    }

    #[test]
    fn a_multi_byte_comment_is_split_on_a_character_boundary() {
        // The quote is one byte and the text after it need not be.
        assert_eq!(
            split_comment("+7776FU,'* コメント"),
            Commented {
                command: "+7776FU",
                comment: Some("'* コメント"),
            }
        );
    }

    #[test]
    fn classifies_anything_else_as_unknown() {
        for line in [
            // The empty line is absent: since #87 it is a keep-alive, and
            // `Unparsed::Unknown("")` is unreachable.
            "junk",
            // Commands are case-sensitive and uppercase.
            "login engine-1 token",
            "logout",
            "agree",
            // A % line that is none of the three special moves.
            "%TORYU",
            "%",
            // A recognized keyword is not a recognized line.
            "LOGOUT now",
            "LOGOUTX",
        ] {
            assert_eq!(Command::parse(line), Err(Unparsed::Unknown(line)));
        }
    }
}
