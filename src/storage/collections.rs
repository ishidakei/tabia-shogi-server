//! Position collections: the plain-text USI file an operator edits, loaded and
//! validated entry by entry.
//!
//! A collection is data, not configuration: one position per line, each line a
//! USI `position` command whose leading `position` keyword may be omitted, so
//! that a collection already published in USI form loads with no conversion
//! step. An entry becomes a [`StartSpec`] here, and nothing above this module
//! ever sees a collection's USI string.
//!
//! A move's USI spelling is [`usi::notation`](crate::usi::notation)'s. What is
//! this module's is everything around the moves: the optional keyword, the
//! base, and the replay that validates an entry.
//!
//! The conversion is the validation: every parsed entry is replayed by
//! [`StartSpec::traversal`], which runs the ordinary legality path.
//!
//! A setup may not pass through the same position [`repetition::OCCURRENCES`]
//! times. Such an entry has its verdict decided before `START` — the game
//! would end `#SENNICHITE` the first time live play re-visits that position,
//! for reasons no client could see in the `Game_Summary`. Three occurrences
//! stay legal: a client still has a move to avoid the fourth.
//!
//! Only the `startpos` base is accepted. A `sfen` base is the extension the
//! format reserves for handicap positions, and is refused by name until that
//! is supported.
//!
//! Every bad line is reported, not the first: failing fast would make a
//! thousand-line collection a thousand restarts.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::game::repetition::{self, PositionKey};
use crate::game::{IllegalSetup, StartSpec};
use crate::usi;

/// The entries of one collection file, in file order.
///
/// A `Collection` exists only if every one of its entries parsed and decoded,
/// which is why the entries are private. An empty collection is an ordinary
/// value here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    entries: Vec<StartSpec>,

    /// Where each entry was written, one-based, parallel to `entries`.
    lines: Vec<usize>,

    /// Each entry's canonical USI line, parallel to `entries` — the identity a
    /// game's starting position is filed and counted under. Here rather than
    /// re-rendered downstream, so that a position has exactly one spelling in
    /// this server and the statistics cannot be grouped under a second one.
    positions: Vec<String>,
}

impl Collection {
    /// Parses a collection's text, or reports every line it refused.
    ///
    /// Text rather than a path, so that the parser is testable with no
    /// filesystem; [`Collection::load`] is the wrapper that reads a file.
    ///
    /// Blank lines — empty or whitespace-only — are skipped, so a trailing
    /// newline is harmless, and `str::lines` strips a trailing CR, so a CRLF
    /// file yields exactly what its LF equivalent does.
    ///
    /// The error is the *full* list, one [`EntryError`] per refused line, with
    /// its one-based line number.
    pub fn parse(text: &str) -> Result<Self, Vec<EntryError>> {
        let mut entries = Vec::new();
        let mut lines = Vec::new();
        let mut positions = Vec::new();
        let mut errors = Vec::new();

        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match parse_entry(line).and_then(validated) {
                Ok(entry) => {
                    entries.push(entry);
                    lines.push(index + 1);
                    positions.push(canonical(line));
                }
                Err(reason) => errors.push(EntryError {
                    line: index + 1,
                    reason,
                }),
            }
        }

        if errors.is_empty() {
            Ok(Self {
                entries,
                lines,
                positions,
            })
        } else {
            Err(errors)
        }
    }

    /// Reads a collection file and parses it.
    ///
    /// A file that is not there, is unreadable, or is not UTF-8 is a
    /// [`LoadError::Read`]: the project is UTF-8 throughout, so a decoding
    /// failure is a property of the file rather than of any one entry.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        Self::parse(&text).map_err(|errors| LoadError::Invalid {
            path: path.to_path_buf(),
            errors,
        })
    }

    /// The entries, in the order the file listed them.
    pub fn entries(&self) -> &[StartSpec] {
        &self.entries
    }

    /// The entries paired with the one-based line they were written on. The
    /// numbering is the file's, blank lines included, so a number here is the
    /// number an operator's editor shows.
    pub fn numbered(&self) -> impl Iterator<Item = (usize, &StartSpec)> {
        self.lines.iter().copied().zip(&self.entries)
    }

    /// Each entry's canonical USI line, parallel to
    /// [`entries`](Self::entries): the identity a starting position carries
    /// everywhere outside this module, with two positions equal exactly when
    /// these strings are equal in full.
    ///
    /// Canonical means the shape [`parse`](Self::parse) settles on: the
    /// leading `position` keyword always present, one space between tokens,
    /// and the tokens themselves as the operator wrote them. So
    /// `startpos moves 7g7f` and `position   startpos   moves 7g7f` reach the
    /// column as the same bytes.
    ///
    /// A collection that lists one line twice has that position twice here,
    /// and the two share their statistics.
    pub fn positions(&self) -> &[String] {
        &self.positions
    }

    /// How many entries the collection holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the collection holds no entry at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One refused line: which line it was, and which rule it broke.
///
/// Nothing borrows the text, so a rejection outlives the file it came from and
/// can be collected with every other one before anything is printed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {reason}")]
pub struct EntryError {
    /// Which line of the file, counted from **one** as an operator's editor
    /// counts them.
    pub line: usize,

    /// Which rule the line broke, carried whole.
    #[source]
    pub reason: EntryReason,
}

/// Why a line is not a usable collection entry.
///
/// One variant per validation rule this loader can decide on its own. The
/// rules that need a configured value — the minimum plies under `Max_Moves`, a
/// handicap entry carrying an asymmetric allowance, a reduction with no move
/// by the reduced side — are `config`'s, because they validate a
/// configuration *against* a loaded collection rather than a line against the
/// format.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EntryReason {
    /// The first token is neither the optional `position` keyword nor a base.
    ///
    /// Distinct from [`EntryReason::UnknownBase`] by what is known of the
    /// operator's intent: with the keyword written, the line is meant as an
    /// entry and the complaint is about its base, which gets named; without
    /// it, the first token could be anything, so the token itself is named.
    #[error(
        "expected a base such as `startpos`, optionally preceded by `position`, found `{found}`"
    )]
    NotAPosition {
        /// The first token, as written.
        found: String,
    },

    /// A line with no base at all — `position` alone.
    #[error("no base; the only base accepted is `startpos`")]
    MissingBase,

    /// A `sfen` base. Refused by name rather than as an unknown base, because
    /// `sfen` is the extension reserved for handicap positions.
    #[error("a `sfen` base is reserved for handicap positions; only `startpos` is accepted")]
    SfenBase,

    /// A base that is neither `startpos` nor `sfen`.
    #[error("unknown base `{base}`; the only base accepted is `startpos`")]
    UnknownBase {
        /// The base, as written.
        base: String,
    },

    /// Something other than `moves` follows the base.
    #[error("expected `moves` after `startpos`, found `{found}`")]
    ExpectedMoves {
        /// The token found in `moves`'s place.
        found: String,
    },

    /// `moves` with no move after it.
    #[error("`moves` with no move after it")]
    NoMoves,

    /// A token that is not a USI move. Case is significant both ways, so
    /// `7G7F` and `p*5f` land here.
    #[error("`{token}` is not a USI move")]
    MalformedMove {
        /// The token, as written.
        token: String,
    },

    /// The sequence is grammatical but the replay refused a move.
    ///
    /// The ordinal printed is one-based, converted from
    /// [`IllegalSetup::index`], which is zero-based.
    #[error("setup move {}: {}", .0.index + 1, .0.reason)]
    IllegalSetup(#[source] IllegalSetup),

    /// The replay passes through one position [`repetition::OCCURRENCES`]
    /// times, which is the count that ends a game.
    ///
    /// The ply is a count of setup moves played, not an ordinal of a move: ply
    /// 0 is the transmitted start, which is occurrence one, and is why a
    /// sequence can reach four occurrences on its third return to the position
    /// it began at.
    #[error(
        "ply {ply} is occurrence {} of a position the setup already passed through; \
         a setup may not pass through the same position {} times, \
         since the game would then end in repetition on the first re-visit",
        repetition::OCCURRENCES,
        repetition::OCCURRENCES
    )]
    RepeatedSetup {
        /// How many setup moves had been played at the offending occurrence,
        /// the transmitted start being ply 0.
        ply: usize,
    },
}

/// Why a collection file could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The file could not be read at all.
    #[error("could not read the position collection {}", .path.display())]
    Read {
        /// The path that was tried.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },

    /// The file was read and entries were refused.
    ///
    /// The message lists every one of them, so that startup fails naming the
    /// offending entries rather than counting them.
    #[error("{} rejected {} of its entries:\n{}", .path.display(), .errors.len(), listed(.errors))]
    Invalid {
        /// The file the entries came from.
        path: PathBuf,
        /// Every refused line, in file order.
        errors: Vec<EntryError>,
    },
}

/// The rejected entries, one per line, for [`LoadError::Invalid`]'s message.
fn listed(errors: &[EntryError]) -> String {
    errors
        .iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The entry, replayed. The positions are the validation and not the product:
/// a game replays its own start when it begins.
///
/// The replay is [`StartSpec::traversal`] rather than [`StartSpec::decode`]
/// because the occurrence rule needs the positions passed through and not only
/// the one arrived at.
///
/// Occurrences are counted the way `Game::new` seeds a game from the same
/// traversal: the transmitted start is the first occurrence, and each
/// traversed position one more.
fn validated(entry: StartSpec) -> Result<StartSpec, EntryReason> {
    let traversal = entry.traversal().map_err(EntryReason::IllegalSetup)?;

    let mut occurrences: HashMap<PositionKey, u32> = HashMap::new();
    for (ply, position) in traversal.iter().enumerate() {
        let count = occurrences.entry(PositionKey::of(position)).or_insert(0);
        *count += 1;
        if *count >= repetition::OCCURRENCES {
            return Err(EntryReason::RepeatedSetup { ply });
        }
    }

    Ok(entry)
}

/// One accepted line, in the one shape this server spells that position in.
///
/// The `position` keyword written whether or not the operator wrote it, then
/// the tokens the parser read, one space between them. Nothing is re-rendered
/// from the parsed moves, so this is a normalization of the text rather than a
/// second encoder; it agrees with
/// [`usi::position_line`](crate::usi::position_line) because each accepted
/// token is the only spelling of its move.
fn canonical(line: &str) -> String {
    let mut tokens = line.split_whitespace().peekable();
    tokens.next_if_eq(&"position");

    let mut canonical = String::from("position");
    for token in tokens {
        canonical.push(' ');
        canonical.push_str(token);
    }

    canonical
}

/// One line of a collection, as far as the grammar goes.
///
/// Legality is not here: `P*5f` is a well-formed drop and an illegal setup at
/// the same time.
///
/// Tokens are taken with `split_whitespace`, so a double or trailing space is
/// not a rejection. Case is not normalized: a collection that loaded here
/// under a lenient reader would fail against every other tool that reads it.
///
/// The leading `position` keyword is optional and consumed at most once, so
/// `position position startpos` names `position` as an unknown base rather
/// than accepting a second keyword.
fn parse_entry(line: &str) -> Result<StartSpec, EntryReason> {
    let mut tokens = line.split_whitespace().peekable();

    let keyword = tokens.next_if_eq(&"position").is_some();

    // A blank line never reaches here — `Collection::parse` skips it — so a
    // line with no base is `position` alone.
    match tokens.next().ok_or(EntryReason::MissingBase)? {
        "startpos" => {}
        "sfen" => return Err(EntryReason::SfenBase),
        base if keyword => {
            return Err(EntryReason::UnknownBase {
                base: base.to_owned(),
            });
        }
        found => {
            return Err(EntryReason::NotAPosition {
                found: found.to_owned(),
            });
        }
    }

    let setup = match tokens.next() {
        // A bare `startpos`, keyword or not, is a buoy with an empty sequence.
        None => Vec::new(),
        Some("moves") => {
            let mut setup = Vec::new();
            for token in tokens {
                setup.push(
                    usi::parse_move(token).ok_or_else(|| EntryReason::MalformedMove {
                        token: token.to_owned(),
                    })?,
                );
            }
            if setup.is_empty() {
                return Err(EntryReason::NoMoves);
            }
            setup
        }
        Some(found) => {
            return Err(EntryReason::ExpectedMoves {
                found: found.to_owned(),
            });
        }
    };

    Ok(StartSpec::Buoy { setup })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Color, HandKind, Illegal, Move, Position, Square};

    /// A three-ply `startpos` entry, the ordinary shape of a collection line.
    const EXAMPLE: &str = "position startpos moves 7g7f 3c3d 2g2f";

    /// An `sfen` base, which the loader reserves and refuses.
    const SFEN_EXAMPLE: &str =
        "position sfen lnsgkgsn1/1r5b1/ppppppppp/9/9/9/PPPPPPPPP/1B5R1/LNSGKGSNL w - 1";

    /// The dummy buoy's four-ply king shuttle, in USI: both kings step out and
    /// back, so the fourth ply returns exactly to hirate.
    const SHUTTLE: &str = "5i5h 5a5b 5h5i 5b5a";

    /// An entry whose setup walks the shuttle `count` times.
    fn shuttles(count: usize) -> String {
        let walk = [SHUTTLE].repeat(count).join(" ");
        format!("position startpos moves {walk}")
    }

    fn parsed(text: &str) -> Collection {
        Collection::parse(text).unwrap_or_else(|errors| panic!("{text:?} was rejected: {errors:?}"))
    }

    fn rejected(text: &str) -> Vec<EntryError> {
        match Collection::parse(text) {
            Err(errors) => errors,
            Ok(collection) => panic!("{text:?} parsed to {collection:?}"),
        }
    }

    /// The single rejection of a one-line text.
    fn sole_rejection(text: &str) -> EntryError {
        let mut errors = rejected(text);
        assert_eq!(errors.len(), 1, "{errors:?}");
        errors.remove(0)
    }

    /// The same entry text with its leading `position ` keyword removed.
    fn without_keyword(text: &str) -> &str {
        text.strip_prefix("position ")
            .expect("the fixture is written with the keyword")
    }

    fn setup_of(entry: &StartSpec) -> &[Move] {
        match entry {
            StartSpec::Buoy { setup } => setup,
            StartSpec::Board(_) => panic!("this loader never produces a written board"),
        }
    }

    fn square_of(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    fn decoded(entry: &StartSpec) -> Position {
        entry
            .decode()
            .unwrap_or_else(|error| panic!("{entry:?} failed to decode: {error}"))
    }

    #[test]
    fn a_bare_startpos_line_is_a_buoy_with_an_empty_sequence() {
        let collection = parsed("position startpos");

        assert_eq!(collection.len(), 1);
        assert!(setup_of(&collection.entries()[0]).is_empty());
        assert_eq!(decoded(&collection.entries()[0]), Position::hirate());
    }

    #[test]
    fn an_entry_parses_equal_with_or_without_the_leading_keyword() {
        let two_shuttles = shuttles(2);

        // The keyword decides nothing about what an accepted line means.
        for text in [EXAMPLE, "position startpos", two_shuttles.as_str()] {
            assert_eq!(parsed(without_keyword(text)), parsed(text), "{text:?}");
        }
    }

    #[test]
    fn a_bare_startpos_with_no_keyword_is_the_same_hirate_entry() {
        let collection = parsed("startpos");

        assert_eq!(collection.len(), 1);
        assert!(setup_of(&collection.entries()[0]).is_empty());
        assert_eq!(decoded(&collection.entries()[0]), Position::hirate());
    }

    #[test]
    fn a_file_mixing_the_two_shapes_loads_as_the_wholly_keyword_ful_one_does() {
        let two_shuttles = shuttles(2);
        let shapes = [
            (without_keyword(EXAMPLE), EXAMPLE),
            ("position startpos", "position startpos"),
            ("startpos", "position startpos"),
            (
                "position startpos moves 2g2f",
                "position startpos moves 2g2f",
            ),
            (without_keyword(&two_shuttles), two_shuttles.as_str()),
        ];

        let mixed: String = shapes
            .iter()
            .map(|(written, _)| *written)
            .collect::<Vec<_>>()
            .join("\n");
        let keyword_ful: String = shapes
            .iter()
            .map(|(_, canonical)| *canonical)
            .collect::<Vec<_>>()
            .join("\n");

        let collection = parsed(&mixed);
        assert_eq!(collection, parsed(&keyword_ful));

        // The line numbers are the file's throughout.
        let numbered: Vec<(usize, usize)> = collection
            .numbered()
            .map(|(line, entry)| (line, setup_of(entry).len()))
            .collect();
        assert_eq!(numbered, [(1, 3), (2, 0), (3, 0), (4, 1), (5, 8)]);
    }

    #[test]
    fn a_three_ply_startpos_entry_parses_and_decodes_with_white_to_move() {
        let collection = parsed(EXAMPLE);
        let entry = &collection.entries()[0];

        assert_eq!(
            setup_of(entry),
            [
                Move::Board {
                    from: square_of(7, 7),
                    to: square_of(7, 6),
                    promote: false,
                },
                Move::Board {
                    from: square_of(3, 3),
                    to: square_of(3, 4),
                    promote: false,
                },
                Move::Board {
                    from: square_of(2, 7),
                    to: square_of(2, 6),
                    promote: false,
                },
            ]
        );

        // Three plies, so gote is to move.
        assert_eq!(decoded(entry).side_to_move(), Color::White);
    }

    #[test]
    fn the_move_grammar_covers_a_board_move_a_promotion_and_a_drop() {
        // `P*5f` is a well-formed drop here and an illegal setup from hirate.
        let entry = parse_entry("position startpos moves 7g7f 8h2b+ P*5f")
            .expect("the line is grammatical");

        assert_eq!(
            setup_of(&entry),
            [
                Move::Board {
                    from: square_of(7, 7),
                    to: square_of(7, 6),
                    promote: false,
                },
                Move::Board {
                    from: square_of(8, 8),
                    to: square_of(2, 2),
                    promote: true,
                },
                Move::Drop {
                    piece: HandKind::Pawn,
                    to: square_of(5, 6),
                },
            ]
        );
    }

    #[test]
    fn a_promoting_setup_move_decodes() {
        // The bishop exchange: 7g7f opens the diagonal, 3c3d clears 3c, and
        // 8h2b+ captures the gote bishop as a horse.
        let collection = parsed("position startpos moves 7g7f 3c3d 8h2b+");
        let position = decoded(&collection.entries()[0]);

        assert!(position.hand(Color::Black).count(HandKind::Bishop) > 0);
    }

    #[test]
    fn an_illegal_setup_names_its_line_and_its_one_based_move_ordinal() {
        // Grammatical, and illegal from hirate: nothing is in hand to drop.
        let error = sole_rejection("position startpos moves P*5f");

        assert_eq!(error.line, 1);
        let EntryReason::IllegalSetup(illegal) = &error.reason else {
            panic!("{:?}", error.reason);
        };
        assert_eq!(illegal.index, 0, "IllegalSetup counts from zero");
        assert!(
            matches!(illegal.reason, Illegal::NotInHand(_)),
            "{:?}",
            illegal.reason
        );

        let message = error.to_string();
        assert!(message.contains("line 1"), "{message}");
        assert!(message.contains("setup move 1"), "{message}");
        assert!(message.contains(&illegal.reason.to_string()), "{message}");
    }

    #[test]
    fn the_source_chain_of_a_rejected_entry_reaches_the_illegal_setup() {
        let error = sole_rejection("position startpos moves 7g7f 7g7f");

        let reason = std::error::Error::source(&error).expect("the reason is the source");
        let illegal = reason.source().expect("the setup rejection is its source");

        let illegal = illegal
            .downcast_ref::<IllegalSetup>()
            .expect("the chain reaches IllegalSetup");
        assert_eq!(illegal.index, 1);
        assert_eq!(
            illegal.reason,
            Illegal::EmptySquare {
                from: square_of(7, 7),
            }
        );
    }

    #[test]
    fn a_setup_passing_through_a_position_three_times_is_accepted() {
        // Two shuttles: hirate at ply 0, ply 4 and ply 8, so three occurrences.
        let collection = parsed(&shuttles(2));

        assert_eq!(collection.len(), 1);
        assert_eq!(decoded(&collection.entries()[0]), Position::hirate());
    }

    #[test]
    fn a_setup_passing_through_a_position_four_times_is_refused_naming_the_ply() {
        // One shuttle more: hirate again at ply 12, which is the sennichite
        // count reached before the game starts.
        let error = sole_rejection(&shuttles(3));

        assert_eq!(error.line, 1);
        assert_eq!(error.reason, EntryReason::RepeatedSetup { ply: 12 });

        let message = error.to_string();
        assert!(message.contains("line 1"), "{message}");
        assert!(message.contains("ply 12"), "{message}");
        assert!(
            message.contains("may not pass through the same position 4 times"),
            "{message}"
        );
    }

    #[test]
    fn the_transmitted_start_is_the_occurrence_that_makes_the_fourth() {
        // The three shuttles above move to hirate only three times -- plies 4,
        // 8 and 12 -- so an entry validated by counting the traversed
        // positions alone would be accepted. It is refused because ply 0
        // counts.
        let entry = parse_entry(&shuttles(3)).expect("the line is grammatical");
        let traversal = entry.traversal().expect("the shuttle is legal from hirate");

        let hirate = PositionKey::of(&Position::hirate());
        let moved_to = traversal
            .iter()
            .skip(1)
            .filter(|position| PositionKey::of(position) == hirate)
            .count();
        assert_eq!(moved_to, 3);

        assert_eq!(
            validated(entry).expect_err("ply 0 makes it four"),
            EntryReason::RepeatedSetup { ply: 12 }
        );
    }

    #[test]
    fn a_repeating_setup_is_reported_alongside_the_other_refusals() {
        let text = [
            EXAMPLE,          // 1, valid
            &shuttles(3),     // 2, four occurrences of hirate
            SFEN_EXAMPLE,     // 3, reserved base
            "not a position", // 4
            &shuttles(2),     // 5, valid -- three occurrences
        ]
        .join("\n");

        let errors = rejected(&text);

        let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
        assert_eq!(lines, [2, 3, 4]);
        assert_eq!(errors[0].reason, EntryReason::RepeatedSetup { ply: 12 });
    }

    #[test]
    fn a_sfen_base_is_rejected_by_name_as_reserved_for_handicap() {
        let error = sole_rejection(SFEN_EXAMPLE);

        assert_eq!(error.reason, EntryReason::SfenBase);
        assert_eq!(error.line, 1);

        let message = error.to_string();
        assert!(message.contains("sfen"), "{message}");
        assert!(message.contains("handicap"), "{message}");
    }

    #[test]
    fn a_keywordless_sfen_base_is_rejected_by_name_just_the_same() {
        // The keyword is optional for every base, so the reserved extension is
        // refused by name in both shapes.
        let error = sole_rejection(without_keyword(SFEN_EXAMPLE));

        assert_eq!(error.reason, EntryReason::SfenBase);
        assert_eq!(error.line, 1);

        let message = error.to_string();
        assert!(message.contains("sfen"), "{message}");
        assert!(message.contains("handicap"), "{message}");
    }

    #[test]
    fn a_base_that_is_neither_startpos_nor_sfen_is_rejected() {
        assert_eq!(
            sole_rejection("position hirate").reason,
            EntryReason::UnknownBase {
                base: "hirate".to_owned(),
            }
        );
        assert_eq!(sole_rejection("position").reason, EntryReason::MissingBase);
    }

    #[test]
    fn a_line_whose_first_token_is_neither_the_keyword_nor_a_base_is_rejected() {
        for (text, found) in [
            ("foo moves 7g7f", "foo"),
            ("not a position", "not"),
            ("7g7f", "7g7f"),
        ] {
            let error = sole_rejection(text);

            assert_eq!(
                error.reason,
                EntryReason::NotAPosition {
                    found: found.to_owned(),
                },
                "{text:?}"
            );
            assert!(error.to_string().contains("line 1"), "{error}");
        }
    }

    #[test]
    fn the_rejection_of_a_stray_line_names_both_accepted_shapes() {
        let message = sole_rejection("not a position").reason.to_string();

        assert!(message.contains("startpos"), "{message}");
        assert!(message.contains("position"), "{message}");
        assert!(message.contains("optionally"), "{message}");
    }

    #[test]
    fn a_second_keyword_is_read_as_an_unknown_base() {
        // The keyword is consumed at most once.
        assert_eq!(
            sole_rejection("position position startpos").reason,
            EntryReason::UnknownBase {
                base: "position".to_owned(),
            }
        );
    }

    #[test]
    fn a_malformed_move_token_is_named() {
        // Case is significant both ways: uppercase where USI writes lowercase,
        // and lowercase where it writes uppercase.
        for token in ["7G7F", "p*5f", "7g7", "7g7f++", "K*5f", "7j7f", "0g7f"] {
            let text = format!("position startpos moves {token}");

            assert_eq!(
                sole_rejection(&text).reason,
                EntryReason::MalformedMove {
                    token: token.to_owned(),
                },
                "{token}"
            );
        }
    }

    #[test]
    fn moves_with_nothing_after_it_is_rejected() {
        assert_eq!(
            sole_rejection("position startpos moves").reason,
            EntryReason::NoMoves
        );
        assert_eq!(
            sole_rejection("position startpos 7g7f").reason,
            EntryReason::ExpectedMoves {
                found: "7g7f".to_owned(),
            }
        );
    }

    #[test]
    fn every_invalid_line_is_reported_with_its_own_line_number() {
        let text = [
            EXAMPLE,                        // 1, valid
            SFEN_EXAMPLE,                   // 2, reserved base
            "position startpos",            // 3, valid
            "startpos moves 7g7f",          // 4, valid -- keyword-less
            "not a position",               // 5
            "position startpos moves 7G7F", // 6
            "position startpos moves P*5f", // 7
        ]
        .join("\n");

        let errors = rejected(&text);

        let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
        assert_eq!(lines, [2, 5, 6, 7]);
        assert!(
            matches!(errors[0].reason, EntryReason::SfenBase),
            "{:?}",
            errors[0]
        );
        assert!(
            matches!(errors[1].reason, EntryReason::NotAPosition { .. }),
            "{:?}",
            errors[1]
        );
        assert!(
            matches!(errors[2].reason, EntryReason::MalformedMove { .. }),
            "{:?}",
            errors[2]
        );
        assert!(
            matches!(errors[3].reason, EntryReason::IllegalSetup(_)),
            "{:?}",
            errors[3]
        );
    }

    #[test]
    fn a_file_with_no_bad_line_yields_every_entry_in_order() {
        let text = ["position startpos", EXAMPLE, "position startpos moves 2g2f"].join("\n");

        let collection = parsed(&text);

        assert_eq!(collection.len(), 3);
        let lengths: Vec<usize> = collection
            .entries()
            .iter()
            .map(|entry| setup_of(entry).len())
            .collect();
        assert_eq!(lengths, [0, 3, 1]);
    }

    #[test]
    fn a_crlf_file_yields_the_same_entries_as_its_lf_equivalent() {
        let lf = format!("{EXAMPLE}\nposition startpos\n");
        let crlf = format!("{EXAMPLE}\r\nposition startpos\r\n");

        assert_eq!(parsed(&crlf), parsed(&lf));
        assert_eq!(parsed(&crlf).len(), 2);
    }

    #[test]
    fn blank_lines_are_skipped_wherever_they_fall() {
        let text = format!("\n   \n{EXAMPLE}\n\t\n");

        let collection = parsed(&text);

        assert_eq!(collection.len(), 1);
        assert_eq!(setup_of(&collection.entries()[0]).len(), 3);
    }

    #[test]
    fn a_blank_line_before_a_bad_one_does_not_shift_its_line_number() {
        let text = format!("\n{EXAMPLE}\n\n{SFEN_EXAMPLE}\n");

        assert_eq!(sole_rejection(&text).line, 4);
    }

    #[test]
    fn every_entry_carries_the_file_line_it_was_written_on() {
        // Blank lines count, so the numbers are the ones an editor shows.
        let text = format!("\n{EXAMPLE}\n\nposition startpos\n\n\nposition startpos moves 2g2f\n");

        let collection = parsed(&text);

        let numbered: Vec<(usize, usize)> = collection
            .numbered()
            .map(|(line, entry)| (line, setup_of(entry).len()))
            .collect();
        assert_eq!(numbered, [(2, 3), (4, 0), (7, 1)]);
    }

    #[test]
    fn the_numbered_entries_are_the_entries() {
        let text = ["position startpos", EXAMPLE].join("\n");

        let collection = parsed(&text);

        let listed: Vec<&StartSpec> = collection.numbered().map(|(_, entry)| entry).collect();
        assert_eq!(listed, collection.entries().iter().collect::<Vec<_>>());
    }

    #[test]
    fn every_entry_carries_the_canonical_line_it_stands_for() {
        // Parallel to the entries, in the same order, and one per entry.
        let text = format!("{EXAMPLE}\nposition startpos\n");

        let collection = parsed(&text);

        assert_eq!(
            collection.positions(),
            [
                "position startpos moves 7g7f 3c3d 2g2f",
                "position startpos"
            ]
        );
        assert_eq!(collection.positions().len(), collection.entries().len());
    }

    #[test]
    fn one_position_written_two_ways_reaches_one_canonical_line() {
        // The keyword is optional and the spacing is the operator's, so the
        // same position is the same bytes however it was typed.
        for written in [
            EXAMPLE,
            without_keyword(EXAMPLE),
            "position   startpos    moves 7g7f   3c3d 2g2f",
            "  startpos moves 7g7f 3c3d 2g2f  ",
        ] {
            let collection = parsed(written);

            assert_eq!(
                collection.positions(),
                ["position startpos moves 7g7f 3c3d 2g2f"],
                "{written:?}",
            );
        }
    }

    #[test]
    fn two_positions_that_differ_in_a_move_are_two_lines() {
        let collection = parsed("position startpos moves 7g7f 3c3d\nposition startpos moves 7g7f");

        assert_ne!(collection.positions()[0], collection.positions()[1]);
    }

    #[test]
    fn a_refused_line_leaves_no_canonical_line_behind() {
        // Only an accepted entry appends to either list.
        let text = format!("{EXAMPLE}\n{SFEN_EXAMPLE}\n");

        assert!(Collection::parse(&text).is_err());

        let accepted = parsed(EXAMPLE);
        assert_eq!(accepted.positions().len(), 1);
    }

    #[test]
    fn a_text_with_no_entry_is_an_empty_collection() {
        for text in ["", "\n", "   \n\n"] {
            let collection = parsed(text);

            assert!(collection.is_empty(), "{text:?}");
            assert_eq!(collection.entries(), []);
        }
    }

    /// A path in the temp directory that no other test writes to.
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tabia-shogi-server-{}-{name}.txt",
            std::process::id()
        ))
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn loading_reads_the_file_and_parses_it() {
        let path = temp_path("collection");
        fs::write(&path, format!("{EXAMPLE}\nposition startpos\n"))
            .expect("the temp file is writable");

        let loaded = Collection::load(&path);
        fs::remove_file(&path).expect("the temp file is removable");

        let collection = loaded.expect("every entry is valid");
        assert_eq!(collection.len(), 2);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn loading_a_file_with_bad_entries_names_the_path_and_every_line() {
        let path = temp_path("invalid-collection");
        fs::write(
            &path,
            format!("{EXAMPLE}\n{SFEN_EXAMPLE}\nnot a position\n"),
        )
        .expect("the temp file is writable");

        let loaded = Collection::load(&path);
        fs::remove_file(&path).expect("the temp file is removable");

        let error = match loaded {
            Err(error) => error,
            Ok(collection) => panic!("the file parsed to {collection:?}"),
        };
        let LoadError::Invalid { errors, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(errors.len(), 2);

        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("line 2"), "{message}");
        assert!(message.contains("line 3"), "{message}");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn loading_a_file_that_is_not_there_names_the_path() {
        let path = temp_path("absent-collection");

        let error = match Collection::load(&path) {
            Err(error) => error,
            Ok(collection) => panic!("an absent file parsed to {collection:?}"),
        };

        assert!(matches!(error, LoadError::Read { .. }), "{error:?}");
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
        assert!(
            std::error::Error::source(&error).is_some(),
            "the io error is the source"
        );
    }
}
