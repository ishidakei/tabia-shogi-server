//! Property-based tests for the CSA protocol layer.
//!
//! This is the layer that reads bytes nobody here wrote, so a panic in it is a
//! dropped connection and on the wrong path a dropped game. The unit tests in
//! `src/csa/` pin down what each well-formed line means; these pin down that no
//! line at all gets an answer other than a value.
//!
//! Four groups:
//!
//! 1. Robustness. Arbitrary lines, arbitrary bytes, and near-valid mutations of
//!    messages a real client sends, put through the framing layer, the
//!    classifier, the comment split and the move parser.
//! 2. Classification. What a classification claims is checked against the line
//!    it came from.
//! 3. Round-trips. Format, parse, format again, and the second format equals the
//!    first.
//! 4. Rendering. Every server response is one line, which is what
//!    `LineWriter::write_line` requires of its argument.
//!
//! Every test here is `#[cfg_attr(miri, ignore)]`: hundreds of randomized cases
//! under an interpreter cost far more than they find, and the two codec
//! properties additionally need the tokio runtime, which miri cannot drive.

mod strategies;

use std::sync::OnceLock;

use proptest::prelude::*;
use proptest::test_runner::TestCaseResult;
use tokio::runtime::{Builder, Runtime};

use strategies::{colors, config, piece_kinds, squares, walks};
use tabia_shogi_server::csa::{
    Command, Commented, LineReader, LineWriter, MAX_LINE_LEN, MoveEcho, ParseError, Response,
    Unparsed, WrittenMove, game_summary, split_comment,
};
use tabia_shogi_server::csa::{GameSummary, TimeSettings, TimeUnit};
use tabia_shogi_server::game::{Color, StartSpec};

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// The characters a CSA line is actually made of, plus the two that decide how
/// one is cut up.
///
/// Uniform `char` generation almost never produces a `+`, a `%` or a `'`, so a
/// suite drawing only from it would spend its whole budget on lines the
/// classifier rejects at the first character.
const ALPHABET: &[char] = &[
    '+', '-', '%', ',', '\'', ' ', '\t', '\r', '0', '1', '5', '7', '9', 'A', 'C', 'D', 'E', 'F',
    'G', 'H', 'I', 'K', 'L', 'M', 'N', 'O', 'R', 'T', 'U', 'Y', 'a', 'x',
];

/// Messages a real client sends, verbatim.
///
/// The mutation strategy edits these rather than generating from nothing: the
/// interesting failures in a hand-written parser are one character away from
/// something valid, and uniform noise never lands there.
const CORPUS: &[&str] = &[
    "+7776FU",
    "-3334FU",
    "+2822UM",
    "+0055FU",
    "+2726FU,'* 56 -8384FU +2625FU -8485FU",
    "LOGIN engine-1 0123456789abcdef",
    "LOGOUT",
    "AGREE 20260824-092500-engine-1-engine-2",
    "REJECT",
    "%TORYO",
    "%KACHI",
    "%CHUDAN",
    "%%GAME tabia-1 +",
    "%%WHO",
    "",
    " ",
    "   ",
];

/// One character, weighted toward the protocol's own alphabet.
fn noisy_chars() -> impl Strategy<Value = char> {
    prop_oneof![
        4 => (0usize..ALPHABET.len()).prop_map(|index| ALPHABET[index]),
        1 => any::<char>(),
    ]
}

/// A framed line: any UTF-8 without the terminator the codec owns.
///
/// Bounded well below [`MAX_LINE_LEN`]: the framing layer's own cap is tested
/// against a full-length line in `src/csa/codec.rs`.
fn lines() -> impl Strategy<Value = String> {
    prop::collection::vec(noisy_chars(), 0..40)
        .prop_map(|chars| chars.into_iter().filter(|&c| c != '\n').collect())
}

/// A real message with one to three characters edited.
fn mutations() -> impl Strategy<Value = String> {
    (
        (0usize..CORPUS.len()).prop_map(|index| CORPUS[index]),
        prop::collection::vec((any::<u8>(), any::<usize>(), noisy_chars()), 1..=3),
    )
        .prop_map(|(base, edits)| {
            let mut chars: Vec<char> = base.chars().collect();

            for (operation, at, character) in edits {
                let len = chars.len();
                match operation % 5 {
                    0 if len > 0 => {
                        chars.remove(at % len);
                    }
                    1 => chars.insert(at % (len + 1), character),
                    2 if len > 0 => chars[at % len] = character,
                    3 if len > 0 => chars.truncate(at % len),
                    // Doubling, which is how a client that forgets its
                    // terminator produces `%TORYO%TORYO`.
                    _ => chars.extend_from_within(..),
                }
            }

            chars.into_iter().filter(|&c| c != '\n').collect()
        })
}

/// A syntactically valid move: any sign, any origin or a drop, any destination,
/// any of the fourteen kinds.
///
/// Not restricted to moves that denote anything: `+0055OU`, a dropped King, is a
/// well-formed CSA line.
fn written_moves() -> impl Strategy<Value = WrittenMove> {
    (
        colors(),
        prop::option::of(squares()),
        squares(),
        piece_kinds(),
    )
        .prop_map(|(color, from, to, kind)| WrittenMove {
            color,
            from,
            to,
            kind,
        })
}

/// An engine name as the charset admits one.
const NAME: &str = "[A-Za-z0-9_@.\\-]{1,24}";

/// A token as the charset admits one, less the apostrophe: a `'` anywhere in a
/// client line starts a comment, so a token holding one could never reach the
/// classifier intact, and the round-trip below runs the line through
/// [`split_comment`] first for exactly that reason.
const TOKEN: &str = "[!-&(-~]{1,24}";

/// A Game_ID: one field, so no space, and no apostrophe on `TOKEN`'s terms.
const GAME_ID: &str = "[A-Za-z0-9_.:+\\-]{1,20}";

/// A client line that is a command, in the exact form a client sends it.
///
/// Every variant of [`Command`] appears, so the round-trip below quantifies over
/// the whole enum.
fn command_lines() -> impl Strategy<Value = String> {
    prop_oneof![
        (NAME, TOKEN).prop_map(|(name, token)| format!("LOGIN {name} {token}")),
        Just("LOGOUT".to_string()),
        prop::option::of(GAME_ID).prop_map(|id| match id {
            Some(id) => format!("AGREE {id}"),
            None => "AGREE".to_string(),
        }),
        prop::option::of(GAME_ID).prop_map(|id| match id {
            Some(id) => format!("REJECT {id}"),
            None => "REJECT".to_string(),
        }),
        written_moves().prop_map(|written| written.to_string()),
        Just("%TORYO".to_string()),
        Just("%KACHI".to_string()),
        Just("%CHUDAN".to_string()),
        "%%[A-Z]{0,8}( [A-Za-z0-9+\\-]{1,12}){0,2}",
        Just(String::new()),
        Just(" ".to_string()),
        "[ \t]{2,8}",
    ]
}

/// A line the codec may be asked to write: no LF, since `write_line` adds the
/// only one, and no trailing CR, since the reader strips exactly that byte
/// before the terminator.
fn framed_lines() -> impl Strategy<Value = String> {
    lines().prop_filter(
        "a trailing CR is the terminator's, and does not survive a round trip",
        |line| !line.ends_with('\r'),
    )
}

// ---------------------------------------------------------------------------
// Shared checks
// ---------------------------------------------------------------------------

/// What every classification claims about the line it came from.
///
/// The session answers `LOGIN:incorrect` to an [`Unparsed::Login`], echoes the
/// line a [`Command::Extension`] carries back in a `##[WARN]` line, and hands a
/// [`Command::Move`]'s text to the notation parser, so a classification carrying
/// a line other than the one that arrived would put the wrong text on the wire.
fn classification_holds(line: &str) -> TestCaseResult {
    match Command::parse(line) {
        Ok(Command::Login { name, token }) => {
            prop_assert_eq!(
                line,
                format!("LOGIN {name} {token}"),
                "a login reassembles into its own line",
            );
        }
        Ok(Command::Logout) => prop_assert_eq!(line, "LOGOUT"),
        Ok(Command::Agree { game_id }) | Ok(Command::Reject { game_id }) => {
            if let Some(id) = game_id {
                prop_assert!(
                    !id.is_empty() && !id.contains(' '),
                    "a Game_ID is one field"
                );
                prop_assert!(line.ends_with(id));
            }
        }
        Ok(Command::Move { line: carried }) => {
            prop_assert_eq!(carried, line, "a move is carried verbatim");
            prop_assert!(
                line.starts_with('+') || line.starts_with('-'),
                "a move is classified by its sign alone",
            );
        }
        Ok(Command::Resign) => prop_assert_eq!(line, "%TORYO"),
        Ok(Command::DeclareWin) => prop_assert_eq!(line, "%KACHI"),
        Ok(Command::Suspend) => prop_assert_eq!(line, "%CHUDAN"),
        Ok(Command::Extension { line: carried }) => {
            prop_assert_eq!(carried, line, "an extension line is echoed verbatim");
            prop_assert!(line.starts_with("%%"));
        }
        Ok(Command::KeepAlive { echo }) => {
            prop_assert_eq!(line, if echo { "" } else { " " });
        }
        Ok(Command::Whitespace) => {
            // The two keep-alive forms are exact literals, so what is left is
            // every other blank line — `"\t"` among them, which is one
            // character long and still not a keep-alive.
            prop_assert!(
                !line.is_empty() && line != " ",
                "the empty line and the single space are keep-alives",
            );
            // Ruby's `\s`, which is what `Command.factory`'s `/^\s*$/` matches:
            // ASCII only, so a line of ideographic spaces is not blank here
            // either.
            prop_assert!(
                line.bytes()
                    .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | 0x0b | 0x0c)),
                "a whitespace line is whitespace on Ruby's reading of it",
            );
        }
        Err(Unparsed::Login(_)) => {
            prop_assert!(
                line.starts_with("LOGIN"),
                "only a LOGIN line is a failed login"
            );
        }
        Err(Unparsed::Unknown(carried)) => {
            prop_assert_eq!(carried, line, "an unknown line is logged as it arrived");
        }
    }

    Ok(())
}

/// What a comment split claims about the line it came from.
///
/// The split is a prefix, an optional one-character separator, and a suffix
/// starting at the first `'`. What is logged as the comment plus what is parsed
/// as the command has to account for the line, with nothing dropped beyond the
/// single separating comma.
fn split_holds(line: &str) -> TestCaseResult {
    let split = split_comment(line);

    prop_assert!(
        line.starts_with(split.command),
        "the command half is a prefix"
    );
    prop_assert!(!split.command.contains('\''), "the split is at the first '");

    match split.comment {
        None => prop_assert_eq!(split.command, line, "no comment means no change"),
        Some(comment) => {
            prop_assert!(comment.starts_with('\''));
            prop_assert!(line.ends_with(comment), "the comment half is a suffix");

            let gap = line.len() - split.command.len() - comment.len();
            prop_assert!(
                gap == 0 || (gap == 1 && line.as_bytes()[split.command.len()] == b','),
                "at most one comma separates the halves",
            );
        }
    }

    // Idempotent, because the command half holds no `'` left to split at.
    prop_assert_eq!(
        split_comment(split.command),
        Commented {
            command: split.command,
            comment: None,
        },
    );

    Ok(())
}

/// The runtime the two codec properties drive, built once per binary.
///
/// A current-thread runtime with no driver enabled: `LineReader` over a byte
/// slice and `LineWriter` over a `Vec<u8>` never yield to a reactor, so the
/// futures complete on the first poll.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();

    RUNTIME.get_or_init(|| {
        Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime needs nothing of the OS")
    })
}

// ---------------------------------------------------------------------------
// Robustness and classification
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config(2048))]

    /// An arbitrary line is classified or refused, and what comes back
    /// describes the line it came from.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn an_arbitrary_line_is_classified_or_refused(line in lines()) {
        classification_holds(&line)?;
        split_holds(&line)?;

        // The move parser sees whatever the classifier called a move, so it is
        // put through the same arbitrary text.
        let _ = WrittenMove::parse(&line);
    }

    /// The same, one to three characters away from a message a real client
    /// sends.
    ///
    /// This is where a near-miss lands: `+777FU` short by a character,
    /// `LOGIN  name token` with the space doubled, `+7776FU '* 30` with the
    /// comment separator wrong.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_near_valid_message_is_classified_or_refused(line in mutations()) {
        classification_holds(&line)?;
        split_holds(&line)?;

        // The path a real move takes: split the comment off first, then parse
        // what is left. Both halves have to survive a mutated line.
        let command = split_comment(&line).command;
        classification_holds(command)?;
        let _ = WrittenMove::parse(command);
    }
}

// ---------------------------------------------------------------------------
// Round-trips
// ---------------------------------------------------------------------------

/// The canonical line for a command.
///
/// [`Command`] has no `Display`, since the server never writes a client line.
/// Every arm here is the form `Command::parse`'s own documentation table gives
/// for that variant.
fn render(command: Command<'_>) -> String {
    match command {
        Command::Login { name, token } => format!("LOGIN {name} {token}"),
        Command::Logout => "LOGOUT".to_string(),
        Command::Agree { game_id: None } => "AGREE".to_string(),
        Command::Agree { game_id: Some(id) } => format!("AGREE {id}"),
        Command::Reject { game_id: None } => "REJECT".to_string(),
        Command::Reject { game_id: Some(id) } => format!("REJECT {id}"),
        Command::Move { line } | Command::Extension { line } => line.to_string(),
        Command::Resign => "%TORYO".to_string(),
        Command::DeclareWin => "%KACHI".to_string(),
        Command::Suspend => "%CHUDAN".to_string(),
        Command::KeepAlive { echo: true } => String::new(),
        Command::KeepAlive { echo: false } => " ".to_string(),
        // Any blank line longer than one space, since the variant records
        // nothing about which one arrived.
        Command::Whitespace => "\t\t".to_string(),
    }
}

proptest! {
    #![proptest_config(config(2048))]

    /// A written move is a fixed point of format → parse → format.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_written_move_survives_format_parse_format(written in written_moves()) {
        let first = written.to_string();

        prop_assert_eq!(first.chars().count(), 7, "a CSA move is seven characters");
        prop_assert_eq!(WrittenMove::parse(&first), Ok(written));
        prop_assert_eq!(
            WrittenMove::parse(&first).map(|round| round.to_string()),
            Ok(first),
        );
    }

    /// A client line is a fixed point of format → parse → format, through the
    /// comment split the session performs first.
    ///
    /// [`Command::Whitespace`] is the one variant that does not close on the
    /// line that arrived: it records that the line was blank and not which blank
    /// line it was. So the fixed point is stated on the format side, with the
    /// stronger "and it is the line the client sent" claim made for every other
    /// variant.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_client_line_survives_format_parse_format(line in command_lines()) {
        // No apostrophe is generated, so the split is a no-op.
        let split = split_comment(&line);
        prop_assert_eq!(split.command, &line);
        prop_assert_eq!(split.comment, None);

        let parsed = Command::parse(&line);
        prop_assert!(parsed.is_ok(), "a generated wire line must parse: {line:?} -> {parsed:?}");
        let parsed = parsed.expect("checked just above");

        let rendered = render(parsed);
        let reparsed = Command::parse(&rendered);
        prop_assert_eq!(reparsed, Ok(parsed), "re-rendering changed the classification");
        prop_assert_eq!(
            reparsed.map(render),
            Ok(rendered.clone()),
            "format -> parse -> format is not a fixed point",
        );

        if parsed != Command::Whitespace {
            prop_assert_eq!(&rendered, &line);
        }
    }

    /// The relay's form: the client's bare move plus the time it consumed.
    ///
    /// The relayed line is still a move to the classifier — a client receiving
    /// it and echoing it back would be sending a move line — and is not a bare
    /// move to the notation parser, which is what makes `,T` the server's suffix
    /// and never the client's.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_relayed_move_is_the_bare_move_with_a_t_suffix(
        written in written_moves(),
        consumed in any::<u32>(),
    ) {
        let bare = written.to_string();
        let relayed = Response::Move(MoveEcho { text: &bare, consumed }).to_string();

        prop_assert_eq!(&relayed, &format!("{bare},T{consumed}"));
        prop_assert_eq!(Command::parse(&relayed), Ok(Command::Move { line: &relayed }));
        prop_assert_eq!(WrittenMove::parse(&relayed), Err(ParseError::Trailing));
    }

    /// Every response this server writes is one line.
    ///
    /// `LineWriter::write_line` adds the only terminator and asserts its
    /// argument carries none. Nothing in the type system stops a response from
    /// rendering two lines out of a field that held a newline, so what is
    /// checked is that no variant introduces one of its own.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn every_response_renders_as_one_line(
        name in NAME,
        game_id in GAME_ID,
        text in "[!-~]{1,32}",
        consumed in any::<u32>(),
    ) {
        let responses = [
            Response::LoginOk { name: &name },
            Response::LoginIncorrect,
            Response::LogoutCompleted,
            Response::Start { game_id: &game_id },
            Response::Rejected { game_id: &game_id, rejector: &name },
            Response::Move(MoveEcho { text: &text, consumed }),
            Response::Declaration { text: &text },
            Response::UnknownCommand { command: &text },
            Response::KeepAlive,
            Response::CUT_OFF,
        ];

        for response in responses {
            let line = response.to_string();
            prop_assert!(!line.contains('\n'), "{response:?} rendered a terminator");
            prop_assert!(!line.contains('\r'), "{response:?} rendered a carriage return");
        }

        prop_assert_eq!(Response::KeepAlive.to_string(), "", "a keep-alive is the empty line");
    }
}

proptest! {
    #![proptest_config(config(256))]

    /// A `Game_Summary` is a fixed point of format → parse → format, in the one
    /// part of it that is not a constant: the setup moves inside the `Position`
    /// block.
    ///
    /// Each move line is taken apart the way a client takes it apart — the `,T`
    /// suffix off, the move text parsed, the result resolved against the
    /// position the replay had reached — and has to come back as the move that
    /// produced it, with the same spelling.
    ///
    /// The two recipients' summaries are checked against each other in the same
    /// case: identical but for `Your_Turn`.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_game_summary_survives_format_parse_format(
        (walk, times) in walks().prop_flat_map(|walk| {
            let plies = walk.setup.len();
            (Just(walk), prop::collection::vec(0u32..3600, plies..=plies))
        }),
        game_id in GAME_ID,
        black in NAME,
        white in NAME,
    ) {
        let start = StartSpec::Buoy { setup: walk.setup.clone() };
        let summary = GameSummary {
            game_id: &game_id,
            black_name: &black,
            white_name: &white,
            max_moves: None,
            time: TimeSettings {
                unit: TimeUnit::Second,
                total_time: Some(600),
                byoyomi: 0,
                increment: Some(2),
                least_time_per_move: 1,
                roundup: false,
            },
            start: &start,
            setup_times: &times,
        };

        let to_black = game_summary::encode(&summary, Color::Black);
        let to_white = game_summary::encode(&summary, Color::White);
        prop_assert!(to_black.is_ok(), "a walked setup encodes: {to_black:?}");
        prop_assert!(to_white.is_ok(), "a walked setup encodes: {to_white:?}");
        let to_black = to_black.expect("checked just above");
        let to_white = to_white.expect("checked just above");

        // One message, one line per element, and the envelope closed.
        for line in &to_black {
            prop_assert!(!line.contains('\n'), "{line:?} is not one line");
        }
        prop_assert_eq!(to_black.first().map(String::as_str), Some("BEGIN Game_Summary"));
        prop_assert_eq!(to_black.last().map(String::as_str), Some("END Game_Summary"));

        // The recipients differ in exactly one line.
        prop_assert_eq!(to_black.len(), to_white.len());
        let differing: Vec<&String> = to_black
            .iter()
            .zip(&to_white)
            .filter(|(mine, theirs)| mine != theirs)
            .map(|(mine, _)| mine)
            .collect();
        prop_assert_eq!(differing.len(), 1, "only Your_Turn may differ: {:?}", differing);
        prop_assert!(differing[0].starts_with("Your_Turn:"));

        // The Position block: hirate's twelve lines, then one per setup move.
        let begin = to_black.iter().position(|line| line == "BEGIN Position");
        let end = to_black.iter().position(|line| line == "END Position");
        prop_assert!(begin.is_some() && end.is_some(), "the block is delimited");
        let block = &to_black[begin.expect("checked") + 1..end.expect("checked")];
        prop_assert_eq!(block.len(), 12 + walk.setup.len());

        let traversal = start.traversal();
        prop_assert!(traversal.is_ok(), "a walked setup replays: {traversal:?}");
        let traversal = traversal.expect("checked just above");

        for (index, mv) in walk.setup.iter().enumerate() {
            let line = &block[12 + index];
            let split = line.rsplit_once(",T");
            prop_assert!(split.is_some(), "{line:?} carries no T-value");
            let (text, consumed) = split.expect("checked just above");

            prop_assert_eq!(consumed.parse::<u32>(), Ok(times[index]));

            let written = WrittenMove::parse(text);
            prop_assert!(written.is_ok(), "{text:?} is not a move: {written:?}");
            let written = written.expect("checked just above");

            prop_assert_eq!(&written.to_string(), text, "the spelling is a fixed point");
            prop_assert_eq!(written.resolve(&traversal[index]), Ok(*mv));
        }
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config(512))]

    /// Arbitrary bytes off the wire are framed or refused, never panicked on.
    ///
    /// `LineReader` is the first thing a connection reaches, and it is handed
    /// whatever the peer sent — invalid UTF-8, a line with no terminator, a
    /// stream that stops mid-line, interior NULs. Each has an [`Error`] variant,
    /// and the claim is that they are the only outcomes besides a line.
    ///
    /// Every line that does come back is checked against the two guarantees the
    /// classifier above it assumes: within the cap, and carrying no terminator.
    ///
    /// [`Error`]: tabia_shogi_server::csa::Error
    #[test]
    #[cfg_attr(miri, ignore)]
    fn framing_survives_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..600)) {
        runtime().block_on(async {
            let mut reader = LineReader::new(&bytes[..]);

            // The loop ends at `Ok(None)` and at any `Err`, and both are
            // terminal for the same reason: a framing error leaves the stream
            // at no line boundary, so there is nothing to resynchronize to and
            // the session closes the connection.
            while let Ok(Some(line)) = reader.read_line().await {
                let line = line.to_string();
                prop_assert!(line.len() <= MAX_LINE_LEN, "the cap was exceeded");
                prop_assert!(!line.contains('\n'), "a framed line carries no terminator");
            }

            Ok(())
        })?;
    }

    /// A written line reads back as itself.
    ///
    /// `LineWriter` adds a terminator and `LineReader` takes one off, so the
    /// pair is a round trip by construction — which is exactly the kind of
    /// claim that stops being true the moment either side learns a second
    /// terminator or an escape. Several lines per case, because the reader's
    /// buffer is reused across calls and a line that leaked into the next one
    /// would only show up on the second read.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_written_line_reads_back_unchanged(
        written in prop::collection::vec(framed_lines(), 0..6),
    ) {
        let read = runtime().block_on(async {
            let mut writer = LineWriter::new(Vec::new());
            for line in &written {
                writer.write_line(line).await.expect("a Vec accepts every write");
            }

            let bytes = writer.into_inner();
            let mut reader = LineReader::new(&bytes[..]);
            let mut read = Vec::new();
            while let Some(line) = reader.read_line().await.expect("what was written frames") {
                read.push(line.to_string());
            }

            read
        });

        prop_assert_eq!(read, written);
    }
}
