//! CSA move notation: one line's move text to a [`Move`] and back.
//!
//! Syntax and resolution only — whether the resolved move is legal is
//! `game::legality`'s question, and the `,T` suffix is `csa/response.rs`'s.
//!
//! The grammar is the CSA standard kifu move format of the specification's own
//! examples, v1.2.1 section 3: sign, from-square, to-square, and two uppercase
//! letters naming the kind as it stands *after* the move. So a promotion is
//! written by naming the promoted kind — `+2822UM`, a bishop arriving at 22 as
//! a horse — and there is no separate promotion marker. A `<from>` of `00` is
//! a drop from hand.
//!
//! Parsing and resolution are separate stages because their failures are
//! classed differently: a line that does not parse is a malformed line, and a
//! line that parses but denotes nothing is (bar
//! [`ResolveError::NotSideToMove`]) an illegal move.
//!
//! This is the only place a square is spelled `77` and a pawn `FU`; `game/`
//! never sees either.

use std::fmt;

use crate::game::{Color, HandKind, Move, PieceKind, Position, Square};

/// Characters in a move: sign, from, to, piece.
const MOVE_LEN: usize = 7;

/// A move as written on the wire, parsed but not yet read against a position.
///
/// The fields carry no invariant between them. A drop of a King is
/// representable — `+0055OU` is a well-formed CSA line denoting an impossible
/// move, and it is [`WrittenMove::resolve`] that has something to say about
/// it, not the grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrittenMove {
    /// The mover, from the leading `+` or `-`.
    pub color: Color,
    /// Where the piece stands, or `None` for a drop from hand.
    pub from: Option<Square>,
    /// Where it goes.
    pub to: Square,
    /// The kind as written — the kind *after* the move.
    pub kind: PieceKind,
}

/// Which square of a move a coordinate was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// The `<from>` field.
    From,
    /// The `<to>` field.
    To,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::From => "from",
            Self::To => "to",
        })
    }
}

/// Why a line is not a move at all.
///
/// Every variant is a protocol error that ends no game. Nothing borrows the
/// line, so a rejection outlives the codec's buffer and can be logged after
/// the next read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Fewer characters than a move has.
    #[error("a move is {} characters, got {got}", MOVE_LEN)]
    Length {
        /// How many characters arrived.
        got: usize,
    },

    /// Text follows the move. A `,T` suffix is the likely cause: the server
    /// appends that on relay, and the client sends the bare form.
    #[error(
        "text follows the {}-character move; the client sends it bare, with no ,T suffix",
        MOVE_LEN
    )]
    Trailing,

    /// The first character is neither `+` nor `-`.
    #[error("a move starts with + or -, got {got:?}")]
    Sign {
        /// The character found in the sign's place.
        got: char,
    },

    /// A square that is not two digits 1–9.
    #[error("the {endpoint} square {}{} is not a file and rank of 1-9", got[0], got[1])]
    Coordinate {
        /// Which square it was.
        endpoint: Endpoint,
        /// The two characters as they arrived.
        got: [char; 2],
    },

    /// A `<to>` of `00`. `00` names a hand, and nothing moves *to* a hand.
    #[error("00 is a hand, not a destination")]
    DropDestination,

    /// Two letters outside the fourteen. A lowercase `+7776fu` lands here too:
    /// the letters are uppercase.
    #[error("{}{} is not one of the fourteen CSA piece names", got[0], got[1])]
    Piece {
        /// The two characters as they arrived.
        got: [char; 2],
    },
}

/// Why a well-formed move denotes nothing in this position.
///
/// Every variant but [`ResolveError::NotSideToMove`] ends the game as an
/// illegal move. Which is which is the session's decision to make from this
/// value; this layer only makes the cases distinguishable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The sign names the side that is not to move: a protocol error that
    /// changes no state.
    #[error("a {written:?} move arrived with {side_to_move:?} to move")]
    NotSideToMove {
        /// The side the sign names.
        written: Color,
        /// The side whose turn it is.
        side_to_move: Color,
    },

    /// The from-square holds nothing.
    #[error("no piece stands on {}{}", from.file(), from.rank())]
    EmptySquare {
        /// The square found empty.
        from: Square,
    },

    /// The written kind is neither the kind standing on the from-square nor
    /// its promoted form, so the text names no move that piece could make.
    #[error("{written:?} names neither the {found:?} on {}{} nor its promotion", from.file(), from.rank())]
    KindMismatch {
        /// Where the piece stands.
        from: Square,
        /// The kind the line wrote.
        written: PieceKind,
        /// The kind actually there.
        found: PieceKind,
    },

    /// A drop of a King or of a promoted kind. Neither has a `Move::Drop` to
    /// build: `game::HandKind` has no value for it.
    #[error("{kind:?} cannot be dropped from hand")]
    UndroppableKind {
        /// The kind the line wrote.
        kind: PieceKind,
    },
}

/// Why a move could not be written down.
///
/// Both variants mean the position handed in does not match the move handed
/// in, which is a stale snapshot on this server's side rather than anything a
/// client sent. They are errors and not panics because a panic on the relay
/// path would take a live game down with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// A board move whose from-square holds nothing, so there is no kind to
    /// write.
    #[error("no piece stands on {}{} to move", from.file(), from.rank())]
    EmptySquare {
        /// The square found empty.
        from: Square,
    },

    /// A promoting move by a kind with no promoted form.
    #[error("the {kind:?} on {}{} has no promoted form", from.file(), from.rank())]
    NotPromotable {
        /// Where the piece stands.
        from: Square,
        /// What is standing there.
        kind: PieceKind,
    },
}

impl WrittenMove {
    /// Parses one bare move, exactly as [`super::Command::Move`] carries it.
    ///
    /// Syntax only: the result may denote nothing at all in any position.
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        // Characters, not bytes, so that a multi-byte line is a length or
        // coordinate error rather than a byte-count accident.
        let mut chars = line.chars();

        // The sign is read before the length is known, so a line with no sign
        // at all -- `7776FU` -- is reported as the missing sign rather than as
        // a length short by one.
        let Some(sign) = chars.next() else {
            return Err(ParseError::Length { got: 0 });
        };
        let color = match sign {
            '+' => Color::Black,
            '-' => Color::White,
            got => return Err(ParseError::Sign { got }),
        };

        let mut rest = ['\0'; MOVE_LEN - 1];
        let mut count = 0;
        for character in chars {
            if count == rest.len() {
                return Err(ParseError::Trailing);
            }
            rest[count] = character;
            count += 1;
        }
        if count < rest.len() {
            return Err(ParseError::Length { got: count + 1 });
        }

        let from = match [rest[0], rest[1]] {
            ['0', '0'] => None,
            digits => Some(parse_square(Endpoint::From, digits)?),
        };
        let to = match [rest[2], rest[3]] {
            ['0', '0'] => return Err(ParseError::DropDestination),
            digits => parse_square(Endpoint::To, digits)?,
        };
        let letters = [rest[4], rest[5]];
        let kind = kind_of(letters).ok_or(ParseError::Piece { got: letters })?;

        Ok(Self {
            color,
            from,
            to,
            kind,
        })
    }

    /// The written form of `mv` as made by `color` in `position`.
    ///
    /// `position` is the position *before* the move, since that is where the
    /// moving piece still stands. The mover is an argument rather than
    /// `position.side_to_move()`, for a caller rendering a setup sequence.
    pub fn of(color: Color, mv: Move, position: &Position) -> Result<Self, RenderError> {
        match mv {
            Move::Drop { piece, to } => Ok(Self {
                color,
                from: None,
                to,
                kind: piece.to_piece_kind(),
            }),
            Move::Board { from, to, promote } => {
                let piece = position
                    .piece_at(from)
                    .ok_or(RenderError::EmptySquare { from })?;
                let kind = if promote {
                    piece.kind.promoted().ok_or(RenderError::NotPromotable {
                        from,
                        kind: piece.kind,
                    })?
                } else {
                    piece.kind
                };
                Ok(Self {
                    color,
                    from: Some(from),
                    to,
                    kind,
                })
            }
        }
    }

    /// The move this text denotes in `position`, or why it denotes none.
    ///
    /// Not a legality check, and not an ownership check: an opponent's piece
    /// still denotes the move. Both are `game::legality`'s, on the [`Move`]
    /// returned here.
    pub fn resolve(&self, position: &Position) -> Result<Move, ResolveError> {
        // The sign is settled before the board is read, so a move by the side
        // not to move is reported as itself rather than as whatever the
        // opponent's half of the position says about the same squares.
        if self.color != position.side_to_move() {
            return Err(ResolveError::NotSideToMove {
                written: self.color,
                side_to_move: position.side_to_move(),
            });
        }

        let Some(from) = self.from else {
            let piece = HandKind::from_piece_kind(self.kind)
                .ok_or(ResolveError::UndroppableKind { kind: self.kind })?;
            return Ok(Move::Drop { piece, to: self.to });
        };

        let found = position
            .piece_at(from)
            .ok_or(ResolveError::EmptySquare { from })?
            .kind;
        let promote = if self.kind == found {
            false
        } else if found.promoted() == Some(self.kind) {
            true
        } else {
            return Err(ResolveError::KindMismatch {
                from,
                written: self.kind,
                found,
            });
        };

        Ok(Move::Board {
            from,
            to: self.to,
            promote,
        })
    }
}

impl fmt::Display for WrittenMove {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = sign_of(self.color);
        let (file, rank) = match self.from {
            Some(square) => (square.file(), square.rank()),
            None => (0, 0),
        };
        let [first, second] = letters_of(self.kind);
        write!(
            f,
            "{sign}{file}{rank}{}{}{first}{second}",
            self.to.file(),
            self.to.rank()
        )
    }
}

/// The character CSA writes for `color`, shared with
/// `super::position_block`'s cells, hand lines and turn line so the two cannot
/// disagree.
pub(super) const fn sign_of(color: Color) -> char {
    match color {
        Color::Black => '+',
        Color::White => '-',
    }
}

/// The two letters naming `kind`.
///
/// Public rather than `pub(super)` because the board the web half draws reads
/// it too: the crate holds exactly one mapping from a kind to its CSA letters.
pub const fn letters_of(kind: PieceKind) -> [char; 2] {
    match kind {
        PieceKind::Pawn => ['F', 'U'],
        PieceKind::Lance => ['K', 'Y'],
        PieceKind::Knight => ['K', 'E'],
        PieceKind::Silver => ['G', 'I'],
        PieceKind::Gold => ['K', 'I'],
        PieceKind::Bishop => ['K', 'A'],
        PieceKind::Rook => ['H', 'I'],
        PieceKind::King => ['O', 'U'],
        PieceKind::PromotedPawn => ['T', 'O'],
        PieceKind::PromotedLance => ['N', 'Y'],
        PieceKind::PromotedKnight => ['N', 'K'],
        PieceKind::PromotedSilver => ['N', 'G'],
        PieceKind::PromotedBishop => ['U', 'M'],
        PieceKind::PromotedRook => ['R', 'Y'],
    }
}

/// The kind `letters` names, or `None` outside the fourteen.
fn kind_of(letters: [char; 2]) -> Option<PieceKind> {
    Some(match letters {
        ['F', 'U'] => PieceKind::Pawn,
        ['K', 'Y'] => PieceKind::Lance,
        ['K', 'E'] => PieceKind::Knight,
        ['G', 'I'] => PieceKind::Silver,
        ['K', 'I'] => PieceKind::Gold,
        ['K', 'A'] => PieceKind::Bishop,
        ['H', 'I'] => PieceKind::Rook,
        ['O', 'U'] => PieceKind::King,
        ['T', 'O'] => PieceKind::PromotedPawn,
        ['N', 'Y'] => PieceKind::PromotedLance,
        ['N', 'K'] => PieceKind::PromotedKnight,
        ['N', 'G'] => PieceKind::PromotedSilver,
        ['U', 'M'] => PieceKind::PromotedBishop,
        ['R', 'Y'] => PieceKind::PromotedRook,
        _ => return None,
    })
}

/// A file digit and a rank digit as a square. `Square::new` is the range
/// check, so the 1–9 bound is not restated here.
fn parse_square(endpoint: Endpoint, digits: [char; 2]) -> Result<Square, ParseError> {
    digit(digits[0])
        .zip(digit(digits[1]))
        .and_then(|(file, rank)| Square::new(file, rank))
        .ok_or(ParseError::Coordinate {
            endpoint,
            got: digits,
        })
}

/// One decimal digit, or `None` for anything else.
fn digit(character: char) -> Option<u8> {
    u8::try_from(character.to_digit(10)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Piece, Position};

    const ALL_KINDS: [PieceKind; 14] = [
        PieceKind::King,
        PieceKind::Rook,
        PieceKind::Bishop,
        PieceKind::Gold,
        PieceKind::Silver,
        PieceKind::Knight,
        PieceKind::Lance,
        PieceKind::Pawn,
        PieceKind::PromotedRook,
        PieceKind::PromotedBishop,
        PieceKind::PromotedSilver,
        PieceKind::PromotedKnight,
        PieceKind::PromotedLance,
        PieceKind::PromotedPawn,
    ];

    fn square(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    fn parse(line: &str) -> WrittenMove {
        WrittenMove::parse(line).expect("the test's own line parses")
    }

    #[test]
    fn parses_a_board_move_by_either_side() {
        assert_eq!(
            WrittenMove::parse("+7776FU"),
            Ok(WrittenMove {
                color: Color::Black,
                from: Some(square(7, 7)),
                to: square(7, 6),
                kind: PieceKind::Pawn,
            })
        );
        assert_eq!(
            WrittenMove::parse("-3334FU"),
            Ok(WrittenMove {
                color: Color::White,
                from: Some(square(3, 3)),
                to: square(3, 4),
                kind: PieceKind::Pawn,
            })
        );
    }

    #[test]
    fn parses_a_drop_as_a_move_with_no_from_square() {
        assert_eq!(
            WrittenMove::parse("+0055KA"),
            Ok(WrittenMove {
                color: Color::Black,
                from: None,
                to: square(5, 5),
                kind: PieceKind::Bishop,
            })
        );
    }

    #[test]
    fn parses_a_promoted_kind_as_written() {
        assert_eq!(parse("+2822UM").kind, PieceKind::PromotedBishop);
    }

    #[test]
    fn every_one_of_the_fourteen_letters_parses_to_its_kind() {
        let table = [
            ("FU", PieceKind::Pawn),
            ("KY", PieceKind::Lance),
            ("KE", PieceKind::Knight),
            ("GI", PieceKind::Silver),
            ("KI", PieceKind::Gold),
            ("KA", PieceKind::Bishop),
            ("HI", PieceKind::Rook),
            ("OU", PieceKind::King),
            ("TO", PieceKind::PromotedPawn),
            ("NY", PieceKind::PromotedLance),
            ("NK", PieceKind::PromotedKnight),
            ("NG", PieceKind::PromotedSilver),
            ("UM", PieceKind::PromotedBishop),
            ("RY", PieceKind::PromotedRook),
        ];
        assert_eq!(table.len(), 14);

        for (letters, kind) in table {
            let line = format!("+7776{letters}");
            assert_eq!(parse(&line).kind, kind, "{line}");
        }
    }

    #[test]
    fn the_letter_table_round_trips_in_both_directions() {
        for kind in ALL_KINDS {
            assert_eq!(kind_of(letters_of(kind)), Some(kind), "{kind:?}");
        }
    }

    #[test]
    fn exactly_fourteen_uppercase_pairs_name_a_piece() {
        let mut accepted = 0;
        for first in b'A'..=b'Z' {
            for second in b'A'..=b'Z' {
                if kind_of([first as char, second as char]).is_some() {
                    accepted += 1;
                }
            }
        }
        assert_eq!(accepted, 14);
    }

    #[test]
    fn rejects_a_line_of_the_wrong_length() {
        assert_eq!(
            WrittenMove::parse("+77FU"),
            Err(ParseError::Length { got: 5 })
        );
        assert_eq!(WrittenMove::parse(""), Err(ParseError::Length { got: 0 }));
        assert_eq!(WrittenMove::parse("+"), Err(ParseError::Length { got: 1 }));
    }

    #[test]
    fn rejects_trailing_text_including_a_consumption_suffix() {
        assert_eq!(WrittenMove::parse("+7776FU,T12"), Err(ParseError::Trailing));
        assert_eq!(WrittenMove::parse("+7776FUX"), Err(ParseError::Trailing));
    }

    #[test]
    fn rejects_a_line_with_no_sign() {
        assert_eq!(
            WrittenMove::parse("7776FU"),
            Err(ParseError::Sign { got: '7' })
        );
    }

    #[test]
    fn rejects_a_coordinate_digit_outside_one_to_nine() {
        assert_eq!(
            WrittenMove::parse("+7076FU"),
            Err(ParseError::Coordinate {
                endpoint: Endpoint::From,
                got: ['7', '0'],
            })
        );
        assert_eq!(
            WrittenMove::parse("+777AFU"),
            Err(ParseError::Coordinate {
                endpoint: Endpoint::To,
                got: ['7', 'A'],
            })
        );
    }

    #[test]
    fn rejects_a_destination_of_double_zero() {
        assert_eq!(
            WrittenMove::parse("+7700FU"),
            Err(ParseError::DropDestination)
        );
    }

    #[test]
    fn rejects_letters_outside_the_fourteen_and_lowercase_ones() {
        assert_eq!(
            WrittenMove::parse("+7776XX"),
            Err(ParseError::Piece { got: ['X', 'X'] })
        );
        assert_eq!(
            WrittenMove::parse("+7776fu"),
            Err(ParseError::Piece { got: ['f', 'u'] })
        );
    }

    #[test]
    fn a_multi_byte_line_is_a_syntax_error_rather_than_a_byte_count_accident() {
        // Seven characters, nineteen bytes.
        assert_eq!(
            WrittenMove::parse("+７７７６ＦＵ"),
            Err(ParseError::Coordinate {
                endpoint: Endpoint::From,
                got: ['７', '７'],
            })
        );
    }

    #[test]
    fn resolves_a_pawn_push_from_hirate() {
        let position = Position::hirate();
        assert_eq!(
            parse("+7776FU").resolve(&position),
            Ok(Move::Board {
                from: square(7, 7),
                to: square(7, 6),
                promote: false,
            })
        );
    }

    #[test]
    fn resolves_a_written_promoted_kind_as_a_promotion() {
        let position = Position::hirate();
        assert_eq!(
            parse("+8822UM").resolve(&position),
            Ok(Move::Board {
                from: square(8, 8),
                to: square(2, 2),
                promote: true,
            })
        );
    }

    #[test]
    fn resolves_a_move_of_an_already_promoted_piece_without_promoting_again() {
        let mut position = Position::hirate();
        position.set_piece_at(
            square(5, 5),
            Some(Piece {
                kind: PieceKind::PromotedBishop,
                color: Color::Black,
            }),
        );
        assert_eq!(
            parse("+5544UM").resolve(&position),
            Ok(Move::Board {
                from: square(5, 5),
                to: square(4, 4),
                promote: false,
            })
        );
    }

    #[test]
    fn resolves_a_drop_of_a_piece_in_hand() {
        let mut position = Position::hirate();
        position.hand_mut(Color::Black).add(HandKind::Bishop);
        assert_eq!(
            parse("+0055KA").resolve(&position),
            Ok(Move::Drop {
                piece: HandKind::Bishop,
                to: square(5, 5),
            })
        );
    }

    #[test]
    fn resolution_does_not_ask_whether_the_hand_holds_the_piece() {
        let position = Position::hirate();
        assert!(position.hand(Color::Black).is_empty());
        assert_eq!(
            parse("+0055KA").resolve(&position),
            Ok(Move::Drop {
                piece: HandKind::Bishop,
                to: square(5, 5),
            })
        );
    }

    #[test]
    fn an_empty_from_square_is_a_resolution_error() {
        let mut position = Position::hirate();
        position.set_piece_at(square(7, 7), None);
        assert_eq!(
            parse("+7775FU").resolve(&position),
            Err(ResolveError::EmptySquare { from: square(7, 7) })
        );
    }

    #[test]
    fn a_kind_matching_neither_the_piece_nor_its_promotion_is_a_resolution_error() {
        let position = Position::hirate();
        assert_eq!(
            parse("+7776KI").resolve(&position),
            Err(ResolveError::KindMismatch {
                from: square(7, 7),
                written: PieceKind::Gold,
                found: PieceKind::Pawn,
            })
        );
    }

    #[test]
    fn a_drop_of_a_king_or_a_promoted_kind_is_a_resolution_error() {
        let position = Position::hirate();
        assert_eq!(
            parse("+0055OU").resolve(&position),
            Err(ResolveError::UndroppableKind {
                kind: PieceKind::King
            })
        );
        assert_eq!(
            parse("+0055TO").resolve(&position),
            Err(ResolveError::UndroppableKind {
                kind: PieceKind::PromotedPawn
            })
        );
    }

    #[test]
    fn a_sign_contradicting_the_side_to_move_is_its_own_error() {
        let position = Position::hirate();
        assert_eq!(
            parse("-3334FU").resolve(&position),
            Err(ResolveError::NotSideToMove {
                written: Color::White,
                side_to_move: Color::Black,
            })
        );
    }

    #[test]
    fn the_sign_is_reported_ahead_of_anything_the_board_would_say() {
        let mut position = Position::hirate();
        position.set_piece_at(square(3, 3), None);
        assert_eq!(
            parse("-3334FU").resolve(&position),
            Err(ResolveError::NotSideToMove {
                written: Color::White,
                side_to_move: Color::Black,
            })
        );
    }

    #[test]
    fn resolution_does_not_judge_ownership() {
        let position = Position::hirate();
        assert_eq!(
            parse("+3334FU").resolve(&position),
            Ok(Move::Board {
                from: square(3, 3),
                to: square(3, 4),
                promote: false,
            })
        );
    }

    #[test]
    fn every_resolved_move_renders_back_to_its_own_line() {
        let mut position = Position::hirate();
        position.hand_mut(Color::Black).add(HandKind::Bishop);

        for line in ["+7776FU", "+8822UM", "+0055KA"] {
            let written = parse(line);
            let resolved = written.resolve(&position).expect("resolves in hirate");
            let rendered =
                WrittenMove::of(written.color, resolved, &position).expect("renders back");

            assert_eq!(rendered, written, "{line}");
            assert_eq!(rendered.to_string(), line);
        }
    }

    #[test]
    fn a_promoting_move_renders_the_promoted_letters() {
        let position = Position::hirate();
        let rendered = WrittenMove::of(
            Color::Black,
            Move::Board {
                from: square(8, 8),
                to: square(2, 2),
                promote: true,
            },
            &position,
        )
        .expect("a bishop stands on 88");

        assert_eq!(rendered.to_string(), "+8822UM");
    }

    #[test]
    fn a_drop_renders_a_double_zero_from_square() {
        let position = Position::hirate();
        let rendered = WrittenMove::of(
            Color::White,
            Move::Drop {
                piece: HandKind::Pawn,
                to: square(5, 5),
            },
            &position,
        )
        .expect("a drop needs nothing from the board");

        assert_eq!(rendered.to_string(), "-0055FU");
    }

    #[test]
    fn rendering_from_an_empty_square_is_an_error_rather_than_a_panic() {
        let mut position = Position::hirate();
        position.set_piece_at(square(7, 7), None);

        assert_eq!(
            WrittenMove::of(
                Color::Black,
                Move::Board {
                    from: square(7, 7),
                    to: square(7, 6),
                    promote: false,
                },
                &position,
            ),
            Err(RenderError::EmptySquare { from: square(7, 7) })
        );
    }

    #[test]
    fn rendering_a_promotion_of_an_unpromotable_kind_is_an_error_rather_than_a_panic() {
        let position = Position::hirate();

        assert_eq!(
            WrittenMove::of(
                Color::Black,
                Move::Board {
                    from: square(4, 9),
                    to: square(4, 8),
                    promote: true,
                },
                &position,
            ),
            Err(RenderError::NotPromotable {
                from: square(4, 9),
                kind: PieceKind::Gold,
            })
        );
    }

    #[test]
    fn every_error_message_names_what_was_wrong() {
        assert_eq!(
            ParseError::Length { got: 5 }.to_string(),
            "a move is 7 characters, got 5"
        );
        assert_eq!(
            ParseError::Sign { got: '7' }.to_string(),
            "a move starts with + or -, got '7'"
        );
        assert_eq!(
            ParseError::Coordinate {
                endpoint: Endpoint::From,
                got: ['7', '0'],
            }
            .to_string(),
            "the from square 70 is not a file and rank of 1-9"
        );
        assert_eq!(
            ParseError::Piece { got: ['X', 'X'] }.to_string(),
            "XX is not one of the fourteen CSA piece names"
        );
        assert_eq!(
            ResolveError::EmptySquare { from: square(7, 7) }.to_string(),
            "no piece stands on 77"
        );
        assert_eq!(
            RenderError::NotPromotable {
                from: square(4, 9),
                kind: PieceKind::Gold,
            }
            .to_string(),
            "the Gold on 49 has no promoted form"
        );
    }

    #[test]
    fn the_command_layers_raw_move_line_parses_here() {
        let command = crate::csa::Command::parse("+7776FU").expect("a move line");
        let crate::csa::Command::Move { line } = command else {
            panic!("the line classified as something other than a move");
        };
        assert_eq!(
            WrittenMove::parse(line).map(|m| m.kind),
            Ok(PieceKind::Pawn)
        );
    }
}
