//! Repetition: how often a position has occurred, and whether one side kept the
//! other in check throughout.
//!
//! shogi-server is authoritative here rather than the specification, which
//! lists `#SENNICHITE` and `#OUTE_SENNICHITE` and pairs their results but
//! defines neither what makes two positions the same nor how perpetual check
//! is decided. `board.rb`'s `update_sennichite` and `oute_sennichite?` govern,
//! and the order of operations below is that source's.
//!
//! Perpetual check is a streak, not a span: the reference does not look back
//! over the moves between the first and the fourth occurrence. It keeps a
//! history per side which one non-checking move by that side clears entirely,
//! and reads a threshold off it at the fourth occurrence. A side that checks
//! on most of its moves but not all therefore reaches an ordinary draw.

use std::collections::HashMap;

use super::legality::in_check;
use super::position::{Color, Position};

/// How many occurrences of one key end the game.
///
/// `board.rb`, `sennichite?`: `@history[to_s] >= 4`.
///
/// Public because the collection loader refuses an entry whose setup already
/// reaches this count, and that rule has no threshold of its own.
pub const OCCURRENCES: u32 = 4;

/// The mover's own streak count that makes the mover the perpetual checker.
///
/// `board.rb`, `oute_sennichite?`: `@sente_history[to_s] >= 4` on Sente's move.
const CHECKER_THRESHOLD: u32 = 4;

/// The opponent's streak count that makes the *opponent* the perpetual checker,
/// the mover being the side escaping.
///
/// `board.rb`, `oute_sennichite?`: `@gote_history[to_s] >= 3` on Sente's move.
/// The asymmetry with [`CHECKER_THRESHOLD`] is in the source: the two counts
/// are taken at different points relative to the move being processed.
const ESCAPER_THRESHOLD: u32 = 3;

/// A position's identity for repetition purposes.
///
/// The key is the board placement, both hands and the side to move, which is
/// exactly what a [`Position`] is. Ply and clock are excluded, and including
/// either would mean no position ever recurs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PositionKey(Position);

impl PositionKey {
    /// The key `position` is counted under.
    pub fn of(position: &Position) -> Self {
        Self(position.clone())
    }
}

/// What a recorded move means for the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The game goes on.
    None,

    /// The fourth occurrence, with neither side checking throughout:
    /// [`Outcome::Repetition`](super::Outcome::Repetition), a draw.
    Draw,

    /// The fourth occurrence reached under continuous check by one side, which
    /// loses: [`Outcome::PerpetualCheck`](super::Outcome::PerpetualCheck).
    PerpetualCheck {
        /// The side that gave the checks, and so loses.
        loser: Color,
    },
}

/// One side's continuous-check history.
///
/// shogi-server makes an empty history non-empty by writing
/// `@sente_history["dummy"] = 1` and then tests `size > 0`. That key is a Ruby
/// stand-in for a missing `bool`, so aliveness is a variant here and every key
/// of the map is a position.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Streak {
    /// This side's last move left the opponent out of check — or it has not
    /// moved yet. Nothing is counted while broken, and nothing is remembered
    /// from before the break.
    #[default]
    Broken,

    /// A run of checks is in progress, with the positions it has passed
    /// through counted.
    Checking(HashMap<PositionKey, u32>),
}

impl Streak {
    /// The move left the opponent in check: the streak runs, and one already
    /// running keeps everything it has counted.
    fn begin(&mut self) {
        if matches!(self, Self::Broken) {
            *self = Self::Checking(HashMap::new());
        }
    }

    /// The move left the opponent out of check: the history is cleared
    /// entirely, which is what makes the check continuous rather than merely
    /// frequent.
    fn clear(&mut self) {
        *self = Self::Broken;
    }

    /// Counts `key`, if this streak is running. A broken one counts nothing:
    /// shogi-server's two trailing `if`s are guarded by `size > 0`.
    fn count(&mut self, key: &PositionKey) {
        if let Self::Checking(counts) = self {
            let occurrences = counts.entry(key.clone()).or_insert(0);
            *occurrences = occurrences.saturating_add(1);
        }
    }

    /// How often `key` has occurred inside the running streak, and zero while
    /// broken — a missing Ruby hash entry reads as zero for the same reason.
    fn count_of(&self, key: &PositionKey) -> u32 {
        match self {
            Self::Broken => 0,
            Self::Checking(counts) => counts.get(key).copied().unwrap_or(0),
        }
    }
}

/// Every occurrence a game has seen, and the two continuous-check histories:
/// a global count that decides whether the game ends and a per-side streak
/// that decides how.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepetitionState {
    global: HashMap<PositionKey, u32>,
    streaks: [Streak; 2],
}

impl RepetitionState {
    /// A game about to be seeded: nothing counted, neither side checking.
    pub fn new() -> Self {
        Self::default()
    }

    /// The transmitted start, before any move.
    ///
    /// One occurrence, and no streak effect: a start is not a move, so there
    /// is no mover to make alive and nothing to clear. The configured starting
    /// position is already one occurrence when real play begins — a dummy buoy
    /// therefore holds two occurrences of hirate before the first real move.
    pub fn count_start(&mut self, position: &Position) {
        self.count(&PositionKey::of(position));
    }

    /// One applied move, `position` being the position it produced.
    ///
    /// `update_sennichite`'s order exactly, and the order matters:
    ///
    /// 1. the resulting position is counted globally;
    /// 2. the mover's streak is made alive or cleared, on whether the move
    ///    left the opponent in check;
    /// 3. both alive streaks then count the resulting position — not only the
    ///    mover's, which is what makes the escaping side's threshold of three
    ///    reachable;
    /// 4. at the fourth global occurrence, the verdict is read.
    ///
    /// The mover is not a parameter: a move flips the side to move, so the
    /// mover is `position.side_to_move().opponent()`.
    pub fn record(&mut self, position: &Position) -> Verdict {
        let key = PositionKey::of(position);
        let mover = position.side_to_move().opponent();
        let occurrences = self.count(&key);

        // Asked of the position after the move, about the side now to move,
        // who is the mover's opponent: shogi-server's `checkmated?(!player)`.
        if in_check(position, position.side_to_move()) {
            self.streak_mut(mover).begin();
        } else {
            self.streak_mut(mover).clear();
        }

        for side in [Color::Black, Color::White] {
            self.streak_mut(side).count(&key);
        }

        if occurrences < OCCURRENCES {
            return Verdict::None;
        }

        if self.streak(mover).count_of(&key) >= CHECKER_THRESHOLD {
            Verdict::PerpetualCheck { loser: mover }
        } else if self.streak(mover.opponent()).count_of(&key) >= ESCAPER_THRESHOLD {
            Verdict::PerpetualCheck {
                loser: mover.opponent(),
            }
        } else {
            Verdict::Draw
        }
    }

    /// Counts one occurrence of `key`, and answers how many there now are.
    ///
    /// Saturating rather than wrapping: a wrapped count would read as a
    /// position that has never occurred.
    fn count(&mut self, key: &PositionKey) -> u32 {
        let occurrences = self.global.entry(key.clone()).or_insert(0);
        *occurrences = occurrences.saturating_add(1);
        *occurrences
    }

    /// One side's streak.
    fn streak(&self, side: Color) -> &Streak {
        &self.streaks[slot(side)]
    }

    /// One side's streak, mutably.
    fn streak_mut(&mut self, side: Color) -> &mut Streak {
        &mut self.streaks[slot(side)]
    }
}

/// Index into a per-side array: `[black, white]`.
const fn slot(side: Color) -> usize {
    match side {
        Color::Black => 0,
        Color::White => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::game::legality::apply_move;
    use crate::game::position::{HandKind, Move, Piece, PieceKind, Square};

    fn sq(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    fn board(from: (u8, u8), to: (u8, u8)) -> Move {
        Move::Board {
            from: sq(from.0, from.1),
            to: sq(to.0, to.1),
            promote: false,
        }
    }

    fn applied(position: &Position, mv: Move) -> Position {
        apply_move(position, mv).unwrap_or_else(|error| panic!("{mv:?} was rejected: {error}"))
    }

    /// The dummy buoy a hirate start uses when it needs a setup sequence to
    /// carry a T-value: both kings step out and back, returning exactly to
    /// hirate with no check anywhere.
    const KING_SHUTTLE: [((u8, u8), (u8, u8)); 4] = [
        ((5, 9), (5, 8)),
        ((5, 1), (5, 2)),
        ((5, 8), (5, 9)),
        ((5, 2), (5, 1)),
    ];

    fn shuttle() -> Vec<Move> {
        KING_SHUTTLE
            .iter()
            .map(|&(from, to)| board(from, to))
            .collect()
    }

    fn shuttles(count: usize) -> Vec<Move> {
        let mut moves = Vec::new();
        for _ in 0..count {
            moves.extend(shuttle());
        }
        moves
    }

    /// A square as `checker` sees it: Black's coordinates, mirrored through
    /// the center for White, so each scenario below is written once and run
    /// from both sides.
    fn mirrored(checker: Color, file: u8, rank: u8) -> Square {
        match checker {
            Color::Black => sq(file, rank),
            Color::White => sq(10 - file, 10 - rank),
        }
    }

    fn step(checker: Color, from: (u8, u8), to: (u8, u8)) -> Move {
        Move::Board {
            from: mirrored(checker, from.0, from.1),
            to: mirrored(checker, to.0, to.1),
            promote: false,
        }
    }

    /// The perpetual-check board, in `checker`'s frame, with the checker to
    /// move: a rook on 6h, the enemy king on 5a, and the checker's own king
    /// out of the way on 9i. Every square the cycle below uses is empty.
    fn perpetual_board(checker: Color) -> Position {
        let mut position = Position::hirate();
        for file in 1..=9 {
            for rank in 1..=9 {
                position.set_piece_at(sq(file, rank), None);
            }
        }

        for (file, rank, kind, color) in [
            (6, 8, PieceKind::Rook, checker),
            (9, 9, PieceKind::King, checker),
            (5, 1, PieceKind::King, checker.opponent()),
        ] {
            position.set_piece_at(mirrored(checker, file, rank), Some(Piece { kind, color }));
        }
        position.set_side_to_move(checker);

        position
    }

    /// The four-ply cycle, from the position with the rook on 4h and the
    /// checker to move: the rook checks on file 5, the king escapes to 4a, the
    /// rook checks on file 4, the king returns to 5a. Every move by the
    /// checker leaves the opponent in check, and no move by the opponent ever
    /// checks the checker.
    fn cycle(checker: Color) -> [Move; 4] {
        [
            step(checker, (4, 8), (5, 8)),
            step(checker, (5, 1), (4, 1)),
            step(checker, (5, 8), (4, 8)),
            step(checker, (4, 1), (5, 1)),
        ]
    }

    /// The same cycle rotated one ply on, as it repeats after [`entry`] has
    /// already put the rook on file 5.
    fn cycle_after_entry(checker: Color) -> [Move; 4] {
        let [check_five, escape, check_four, back] = cycle(checker);
        [escape, check_four, back, check_five]
    }

    /// The move that enters the cycle from [`perpetual_board`]: the rook's first
    /// check, played from a square the cycle never revisits.
    fn entry(checker: Color) -> Move {
        step(checker, (6, 8), (5, 8))
    }

    /// The checking line: the entry, then `cycles` repetitions of the cycle it
    /// leads into.
    fn checking_line(checker: Color, cycles: usize) -> Vec<Move> {
        let mut moves = vec![entry(checker)];
        for _ in 0..cycles {
            moves.extend(cycle_after_entry(checker));
        }
        moves
    }

    /// The position the cycle closes at: the rook on 4h, the enemy king back on
    /// 5a, and the checker to move.
    fn cycle_start(checker: Color) -> Position {
        let mut position = applied(&perpetual_board(checker), entry(checker));
        for mv in cycle_after_entry(checker).iter().take(3) {
            position = applied(&position, *mv);
        }
        assert_eq!(position.side_to_move(), checker, "the cycle closes here");

        position
    }

    /// Plays `moves` from `position`, recording each, and stops at the first
    /// verdict that is not [`Verdict::None`]. Returns the verdict and how many
    /// moves were played to reach it.
    fn play(state: &mut RepetitionState, position: &Position, moves: &[Move]) -> (Verdict, usize) {
        let mut position = position.clone();
        for (played, &mv) in moves.iter().enumerate() {
            position = applied(&position, mv);
            match state.record(&position) {
                Verdict::None => {}
                verdict => return (verdict, played + 1),
            }
        }

        (Verdict::None, moves.len())
    }

    #[test]
    fn a_position_reached_at_different_plies_is_one_key() {
        // The shuttle returns exactly to hirate.
        let hirate = Position::hirate();
        let mut position = hirate.clone();
        for mv in shuttle() {
            position = applied(&position, mv);
        }

        assert_eq!(PositionKey::of(&position), PositionKey::of(&hirate));
    }

    #[test]
    fn the_side_to_move_and_the_hands_are_part_of_the_identity() {
        let hirate = Position::hirate();

        let mut waiting = hirate.clone();
        waiting.set_side_to_move(Color::White);
        assert_ne!(PositionKey::of(&waiting), PositionKey::of(&hirate));

        let mut holding = hirate.clone();
        holding.hand_mut(Color::Black).add(HandKind::Pawn);
        assert_ne!(PositionKey::of(&holding), PositionKey::of(&hirate));
    }

    #[test]
    fn the_fourth_occurrence_of_a_quiet_position_is_a_draw() {
        // Hirate counted once as the start, then three shuttles: the fourth
        // occurrence falls on the last move of the third.
        let hirate = Position::hirate();
        let mut state = RepetitionState::new();
        state.count_start(&hirate);

        assert_eq!(play(&mut state, &hirate, &shuttles(3)), (Verdict::Draw, 12));
    }

    #[test]
    fn three_occurrences_are_not_enough() {
        let hirate = Position::hirate();
        let mut state = RepetitionState::new();
        state.count_start(&hirate);

        assert_eq!(play(&mut state, &hirate, &shuttles(2)), (Verdict::None, 8));
    }

    #[test]
    fn a_start_that_is_not_counted_needs_one_repetition_more() {
        // The same twelve moves with no `count_start` reach three occurrences,
        // not four: the transmitted start is an occurrence, and skipping it
        // undercounts by one.
        let hirate = Position::hirate();
        let mut state = RepetitionState::new();

        assert_eq!(play(&mut state, &hirate, &shuttles(3)), (Verdict::None, 12));

        // The very next move ends it instead: every position the shuttle
        // passes through has now occurred three times, so the first of a fourth
        // shuttle is a fourth occurrence.
        assert_eq!(play(&mut state, &hirate, &shuttle()), (Verdict::Draw, 1));
    }

    #[test]
    fn counting_the_start_touches_neither_streak() {
        // A start that happens to be a check begins nobody's streak.
        let mut state = RepetitionState::new();
        let checked = applied(&perpetual_board(Color::Black), entry(Color::Black));

        state.count_start(&checked);

        for side in [Color::Black, Color::White] {
            assert_eq!(state.streak(side), &Streak::Broken, "{side:?}");
        }
    }

    #[test]
    fn continuous_check_loses_for_the_checker_at_its_own_threshold() {
        for checker in [Color::Black, Color::White] {
            // The state starts one ply before the cycle, so the checked
            // position first occurs inside the streak and the checker's history
            // keeps pace with the global count.
            let start = perpetual_board(checker);
            let mut state = RepetitionState::new();
            state.count_start(&start);

            assert_eq!(
                play(&mut state, &start, &checking_line(checker, 3)),
                (Verdict::PerpetualCheck { loser: checker }, 13),
                "{checker:?} checking"
            );
        }
    }

    #[test]
    fn one_check_short_of_the_threshold_is_not_yet_a_verdict() {
        for checker in [Color::Black, Color::White] {
            let start = perpetual_board(checker);
            let mut state = RepetitionState::new();
            state.count_start(&start);

            // The same line one ply short: three occurrences of every key.
            let mut moves = checking_line(checker, 3);
            moves.pop();

            assert_eq!(
                play(&mut state, &start, &moves),
                (Verdict::None, 12),
                "{checker:?} checking"
            );
        }
    }

    #[test]
    fn the_escaping_side_wins_at_the_opponents_threshold_of_three() {
        for checker in [Color::Black, Color::White] {
            // The state starts on the cycle, so the position with the checker
            // to move carries the start's own occurrence as a head start and
            // reaches four first — on a move by the escaping side, whose own
            // streak that very move clears. The checker's three counts of that
            // key were every one of them made on a move by its opponent, so an
            // implementation that counted a resulting position into the mover's
            // history alone would read zero here and answer `Draw`.
            let start = cycle_start(checker);
            let mut state = RepetitionState::new();
            state.count_start(&start);

            let mut moves = Vec::new();
            for _ in 0..3 {
                moves.extend(cycle(checker));
            }

            assert_eq!(
                play(&mut state, &start, &moves),
                (Verdict::PerpetualCheck { loser: checker }, 12),
                "{checker:?} checking"
            );
        }
    }

    #[test]
    fn one_quiet_move_clears_the_streak_entirely_and_the_same_repetition_draws() {
        for checker in [Color::Black, Color::White] {
            // The scenario above with one cycle replaced by a detour that ends
            // where it began, so the fourth occurrence still falls on the
            // twelfth ply — but the checker's history was cleared on the way,
            // so it holds one occurrence rather than three. Only a history
            // cleared entirely produces this: one that decremented, or that
            // kept anything from before the break, would still reach three.
            let start = cycle_start(checker);
            let detour = [
                step(checker, (9, 9), (8, 9)),
                step(checker, (5, 1), (6, 1)),
                step(checker, (8, 9), (9, 9)),
                step(checker, (6, 1), (5, 1)),
            ];

            let mut moves = Vec::new();
            moves.extend(cycle(checker));
            moves.extend(detour);
            moves.extend(cycle(checker));

            let mut state = RepetitionState::new();
            state.count_start(&start);

            assert_eq!(
                play(&mut state, &start, &moves),
                (Verdict::Draw, 12),
                "{checker:?} checking"
            );
        }
    }

    #[test]
    fn a_streak_survives_the_opponents_moves_and_the_opponents_never_begins() {
        // The clearing is per side: a move by the escaping side never touches
        // the checker's history.
        let checker = Color::Black;
        let start = perpetual_board(checker);
        let mut state = RepetitionState::new();
        state.count_start(&start);

        let (verdict, _) = play(&mut state, &start, &checking_line(checker, 1));

        assert_eq!(verdict, Verdict::None);
        assert!(
            matches!(state.streak(checker), Streak::Checking(_)),
            "{:?}",
            state.streak(checker)
        );
        assert_eq!(state.streak(checker.opponent()), &Streak::Broken);
    }
}
