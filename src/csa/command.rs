//! Client lines as typed commands.
//!
//! One framed line in, one classification out: a command the session can act
//! on, a `LOGIN` that failed, or a line this server does not recognize.
//! Producing the replies is the session's job. [`split_comment`] runs before
//! any of it, because a comment is text this server does not read.

use super::codec::MAX_LINE_LEN;

/// Longest engine name accepted in a `LOGIN`, in characters.
///
/// shogi-server's identifier set, at length 1–1024. Its own default of 32 is
/// too tight for real engine names and it treats the limit as configurable
/// (`--max-identifier`), so a client that works against shogi-server works
/// against this bound too.
pub const MAX_ENGINE_NAME_LEN: usize = 1024;

/// Longest token accepted in a `LOGIN`, in characters.
///
/// Printable ASCII excluding space, at length 1–64, compared byte for byte.
pub const MAX_TOKEN_LEN: usize = 64;

/// Raising either bound past what the codec will carry makes a valid login
/// unreachable: the codec refuses the line before this parser sees it.
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
/// [`split_comment`] before anything has decided it is a login, so no field
/// here is known to hold credential material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Commented<'a> {
    /// What is left to parse: everything before the first `'`, with one
    /// trailing `,` removed. The whole line when it carried no comment.
    pub command: &'a str,

    /// The comment, from its `'` to the end of the line, or `None` when there
    /// was none. Carried for a log record and nothing else: this server does
    /// not store it, and never relays it.
    pub comment: Option<&'a str>,
}

/// Splits one client line into the part to parse and the comment.
///
/// A `'` anywhere in a client line starts a comment. The CSA server protocol
/// v1.2.1 has no comment syntax; this is the floodgate convention, which
/// shogi-server's `bin/usiToCsa.rb` appends to every move once its engine
/// reports an evaluation, with no option to turn it off, and which
/// shogi-server accepts (`shogi_server/command.rb`, `MoveCommand#call`). A
/// server that classed the line as malformed would drop every move such a
/// client sends and lose it the game on time.
///
/// The rule is the whole line's, not the move line's: `%TORYO,'* bye` is a
/// resignation, and a `LOGIN` is split like anything else — so an `open`-mode
/// token cannot contain a `'`.
///
/// One trailing `,` is removed with the comment, and only that one. Nothing
/// else about the remaining text is repaired, so `+7776FU '* 30` — a space
/// where the comma should be — leaves `+7776FU ` and stays the malformed line
/// shogi-server also made it.
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
/// from: the codec reuses its buffer, so a command must be consumed before the
/// next `read_line()`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    /// `LOGIN <username> <password>` — both fields validated against their
    /// charsets and lengths.
    Login {
        /// The engine name, matching `[A-Za-z0-9_@\-\.]{1,1024}`.
        name: &'a str,
        /// The presented token. Whether it is a real one is `auth`'s question.
        token: &'a str,
    },

    /// `LOGOUT`.
    Logout,

    /// `AGREE [<GameID>]`. The Game_ID is opaque here; whether it names the
    /// offered game is session logic.
    Agree { game_id: Option<&'a str> },

    /// `REJECT [<GameID>]`, on the same terms as [`Command::Agree`].
    Reject { game_id: Option<&'a str> },

    /// A board move, carried raw — `+7776FU` from Black, `-3334FU` from White.
    ///
    /// Classification is by the leading `+` or `-` alone, so a line that
    /// merely starts like a move (a bare `+`) is a move to this layer and is
    /// rejected by the notation layer.
    Move { line: &'a str },

    /// `%TORYO` — resignation, which ends the game `#RESIGN`.
    Resign,

    /// `%KACHI` — a jishogi declaration: `#JISHOGI` if valid,
    /// `#ILLEGAL_MOVE` if not).
    DeclareWin,

    /// `%CHUDAN` — a suspension request. Recognized here because clients send
    /// it; suspension itself is not supported, and the session layer
    /// adjudicates the line as an illegal move by its sender.
    Suspend,

    /// A shogi-server extension command: any `%%` line, `%%GAME` and
    /// `%%SETBUOY` and `%%WHO` alike. Recognized, never implemented — the
    /// server answers `##[WARN] unknown command: <command>` and keeps the
    /// connection open, echoing the line carried here.
    Extension { line: &'a str },

    /// A keep-alive: the empty line, or a line holding exactly one space. An
    /// application-level protocol shogi-server names in `command.rb`, above
    /// its `SpecialCommand`; the two forms differ in the reply and in nothing
    /// else.
    ///
    /// Its 30-second rule — a client may not send a second empty line within
    /// 30 seconds — is not implemented, as in the reference. A keep-alive is
    /// never counted as a malformed line, so a client that sends them faster
    /// than the rule would allow is not disconnected for it.
    KeepAlive {
        /// Whether this one is owed an empty line back — `true` for `""`,
        /// `false` for `" "`.
        echo: bool,
    },

    /// A line that is whitespace and nothing else, longer than the single
    /// space above: shogi-server's `SpaceCommand`, ignored with no reply.
    ///
    /// Not a keep-alive. It earns no reply and no side effect, where a
    /// keep-alive earns the deadline check its state has.
    Whitespace,
}

/// Whether `line` is whitespace and nothing else, on Ruby's reading of `\s`.
///
/// ASCII-only. `/^\s*$/` in `Command.factory` matches `[ \t\r\n\f\v]` and no
/// more, so a line of ideographic spaces is not blank to the reference, where
/// [`char::is_whitespace`] would have made it blank here. The codec admits any
/// UTF-8, so the difference is reachable.
///
/// `\r` and `\n` can only appear as content: the framing layer has already
/// taken the terminator off.
fn is_blank(line: &str) -> bool {
    line.chars()
        .all(|c| matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{b}' | '\u{c}'))
}

/// Hand-written because [`Command::Login`] holds a token and a derived `Debug`
/// would print it. No credential material in a rendering.
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
/// Unlike a framing error this is not fatal to the connection: the
/// malformed-line rule closes only on repeated occurrences, which is a count
/// the session keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unparsed<'a> {
    /// A `LOGIN` line that is not a valid login. The session answers
    /// `LOGIN:incorrect`, where an unknown line is answered with silence.
    Login(LoginRejection<'a>),

    /// Any other unrecognized line, carried as received so the session can log
    /// what actually arrived.
    ///
    /// This is the one place a mistyped credential can still reach a log: a
    /// client that sends `login <name> <token>` in lowercase produces an
    /// unknown line containing its token. Nothing here can tell that apart
    /// from junk, and the log needs the line to reconstruct what happened.
    Unknown(&'a str),
}

/// Why a `LOGIN` failed, carrying no token text in any variant.
///
/// `Debug` is derived, which is only safe because of that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginRejection<'a> {
    /// Not exactly three fields separated by single spaces — a bare `LOGIN`, a
    /// missing token, a doubled space, or shogi-server's
    /// `LOGIN <name> <password> x1`. There is no x1 here, so the trailing
    /// field is a failed login rather than a mode switch.
    ///
    /// Nothing is carried. With the arity wrong, the field in the name
    /// position is whatever the client typed — `LOGIN s3cret` puts a token
    /// there — so there is no field here known not to be a credential.
    Arity,

    /// The engine name failed its charset or length. The offending text is
    /// not carried, for the same reason.
    Name,

    /// The engine name is well formed and the token is not.
    ///
    /// The only variant that carries a name, and the only case that can: three
    /// fields were present and the second validated as a name, so the third is
    /// known to be the token and the second known not to be.
    Token { name: &'a str },
}

impl<'a> Command<'a> {
    /// Classifies one framed line.
    ///
    /// The line arrives from [`super::LineReader`] with its terminator
    /// stripped, its length bounded, and its encoding checked. Commands are
    /// case-sensitive and uppercase, per the specification.
    ///
    /// The empty line is a command, not a malformed line. `Command.factory` in
    /// shogi-server's `command.rb` classifies `""` and `" "` before it
    /// consults the player's status, and a longer whitespace-only line becomes
    /// its `SpaceCommand`. A server that counted the empty line toward
    /// `[csa].max_malformed_lines` would disconnect a client using it as a
    /// keep-alive.
    pub fn parse(line: &'a str) -> Result<Self, Unparsed<'a>> {
        match line {
            "" => return Ok(Self::KeepAlive { echo: true }),
            " " => return Ok(Self::KeepAlive { echo: false }),
            blank if is_blank(blank) => return Ok(Self::Whitespace),
            _ => {}
        }

        if line.starts_with('+') || line.starts_with('-') {
            return Ok(Self::Move { line });
        }
        if line.starts_with("%%") {
            return Ok(Self::Extension { line });
        }

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

    /// Validates `LOGIN <username> <password>`: the engine name's charset and
    /// length, then the token's.
    fn parse_login(line: &'a str) -> Result<Self, Unparsed<'a>> {
        // `split(' ')`, never `split_whitespace`: the specification separates
        // fields with single spaces, and collapsing runs would silently accept
        // `LOGIN  name  token`.
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

/// The engine name: `[A-Za-z0-9_@\-\.]`, 1–1024 characters.
///
/// The length is counted in characters. Every character the charset admits is
/// one byte, so `len()` is that count.
pub fn is_engine_name(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= MAX_ENGINE_NAME_LEN
        && field
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '-' | '.'))
}

/// The token: printable ASCII excluding space — `0x21..=0x7E` — 1–64
/// characters.
///
fn is_token(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= MAX_TOKEN_LEN
        && field.chars().all(|c| matches!(c, '!'..='~'))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Empty fields keep the arity at three, so each field's own charset
        // test is what has to catch them.
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
            // A doubled space is a fourth field, not a wider separator.
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
        // shogi-server reads the trailing field as an extension switch.
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
        // With two fields the token sits where a name would, so a rejection
        // that echoed "the name" would echo the credential.
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

    /// A move line as shogi-server's own bridge sends it, comment and all.
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
        // A token containing a `'` is truncated at it, and what remains is the
        // token presented.
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
        // A second comma is content the line keeps.
        assert_eq!(split_comment("+7776FU,'x").command, "+7776FU");
        assert_eq!(split_comment("+7776FU,,'x").command, "+7776FU,");
    }

    #[test]
    fn a_space_before_the_comment_leaves_a_line_that_still_does_not_parse() {
        // shogi-server makes this form illegal too; the trailing space is what
        // keeps it that way here.
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
        // its own for a line that had nothing in front of its comment.
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
        // Every literal here is one the codec can produce: a trailing CR goes
        // with the terminator, so `"\r "` is a line and `"\r"` is not.
        for line in ["  ", "\t", " \t ", "\t\t", "\r ", "\u{b}", "\u{c}"] {
            assert_eq!(Command::parse(line), Ok(Command::Whitespace), "{line:?}");
        }
    }

    #[test]
    fn a_non_ascii_space_is_not_a_blank_line() {
        // Ruby's `\s` is ASCII-only, so the reference makes these unknown lines
        // and so does this.
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
            "junk",
            "login engine-1 token",
            "logout",
            "agree",
            "%TORYU",
            "%",
            "LOGOUT now",
            "LOGOUTX",
        ] {
            assert_eq!(Command::parse(line), Err(Unparsed::Unknown(line)));
        }
    }
}
