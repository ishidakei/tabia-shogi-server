//! The live-game registry: what a game task publishes, and what a request
//! reads.
//!
//! Reading an in-progress game must never delay move relay, and there is no
//! shared game state to make that a matter of care: the web layer holds a
//! `watch::Receiver` and nothing else, and the game task owns its state
//! outright and publishes an immutable snapshot after each move.
//!
//! So a [`GameSnapshot`] is a copy, complete in itself. A reader that stalled
//! for a minute would delay nothing: the writer's next [`send`] overwrites a
//! slot rather than waiting for anyone to have read the last one.
//!
//! The one shared object is [`Registry`], the map from a `Game_ID` to a
//! channel: a `HashMap` behind an `RwLock`, written twice in a game's life and
//! read for as long as it takes to clone a receiver out. Nothing awaits while
//! holding it, and no relay path touches it.
//!
//! Registration is an owned handle: [`Registry::register`] returns a [`Live`],
//! and dropping it deregisters, so a game task that panics leaves no entry
//! behind.
//!
//! [`send`]: watch::Sender::send

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use tokio::sync::watch;

use crate::game::Position;

/// One in-progress game, as of one moment.
///
/// The snapshot's shape, with three adjustments the page it feeds
/// asked for and one that the wire decided:
///
/// - The two engine names and the start time ride along, because a list row
///   and a page header need them.
/// - `last_move` is the line as it was relayed — `+7776FU,T1` — rather than a
///   [`Move`](crate::game::Move): rendering a move needs the position it was
///   played from, which this snapshot no longer holds, and the task already
///   has the exact text both clients were sent.
/// - The side to move is not a field: it is [`Position::side_to_move`], and a
///   copy beside the position is a copy that can disagree with it.
///
/// `clocks` is `[black, white]`, as a duration each: the game holds unit
/// counts because every value written to the wire is one, and a page shows a
/// time.
///
/// Cheap to clone, because every read of this registry clones one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameSnapshot {
    /// The CSA `Game_ID`.
    pub game_id: String,

    /// Black's engine name, as given at `LOGIN`.
    pub black_name: String,

    /// White's engine name.
    pub white_name: String,

    /// When `START` went out, RFC 3339 in UTC — the same string the row's
    /// `started_at` will carry, formatted from the same moment.
    pub started_at: String,

    /// How many moves the position has been through, setup entries included:
    /// the ply numbering `PlayedMove::ply` uses.
    ///
    /// [`PlayedMove::ply`]: crate::session::PlayedMove::ply
    pub ply: u32,

    /// The position as of this snapshot.
    pub position: Position,

    /// The move line last relayed, `<move>,T<n>`, or `None` before the first
    /// one.
    pub last_move: Option<String>,

    /// What each side has left, `[black, white]`.
    pub clocks: [Duration; 2],
}

/// Every game in progress, by `Game_ID`.
///
/// Shared state, held in an [`Arc`] by the coordinator and by the web layer
/// alike.
#[derive(Debug, Default)]
pub struct Registry {
    live: RwLock<HashMap<String, watch::Receiver<GameSnapshot>>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a game at its `START` and returns the handle it publishes
    /// through.
    ///
    /// The snapshot handed over is the game's first: `START` has gone out, the
    /// setup moves are already in the history, and both clocks hold what the
    /// summary announced. A page requested before the first move renders that.
    ///
    /// A `Game_ID` is minted once per game, so a registration cannot collide
    /// with a live one; if it somehow did, the newer channel replaces the
    /// older.
    pub fn register(self: &Arc<Self>, snapshot: GameSnapshot) -> Live {
        let game_id = snapshot.game_id.clone();
        let (sender, receiver) = watch::channel(snapshot);
        self.write().insert(game_id.clone(), receiver);

        Live {
            registry: Arc::clone(self),
            sender,
            game_id,
        }
    }

    /// `game_id`'s latest snapshot, or `None` if no such game is in progress.
    ///
    /// The clone is the point: the borrow of the channel's slot is released
    /// before this returns, so a caller holds nothing the writer can be
    /// delayed by.
    pub fn get(&self, game_id: &str) -> Option<GameSnapshot> {
        let receiver = self.read().get(game_id).cloned()?;
        let snapshot = receiver.borrow().clone();

        Some(snapshot)
    }

    /// Every game in progress, newest start first.
    ///
    /// Ordered here rather than by the caller, because newest first is the
    /// order the finished half of the list is read in. Ties break on the
    /// `Game_ID`, whose sequence within a round is the order the games were
    /// made in.
    pub fn live(&self) -> Vec<GameSnapshot> {
        let receivers: Vec<watch::Receiver<GameSnapshot>> = self.read().values().cloned().collect();

        let mut games: Vec<GameSnapshot> = receivers
            .iter()
            .map(|receiver| receiver.borrow().clone())
            .collect();
        games.sort_by(|one, other| {
            other
                .started_at
                .cmp(&one.started_at)
                .then_with(|| other.game_id.cmp(&one.game_id))
        });

        games
    }

    /// How many games are in progress.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether no game is in progress.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets `game_id`. [`Live`]'s drop, and nothing else, calls this.
    fn deregister(&self, game_id: &str) {
        self.write().remove(game_id);
    }

    /// The map, for reading.
    ///
    /// A poisoned lock is taken anyway. The invariant a panic could have broken
    /// is a `HashMap`'s, and nothing between these two lines can panic — the
    /// alternative is a server whose game list is permanently unavailable
    /// because one unrelated task unwound.
    fn read(&self) -> RwLockReadGuard<'_, HashMap<String, watch::Receiver<GameSnapshot>>> {
        self.live.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The map, for writing, on [`read`](Self::read)'s terms.
    fn write(&self) -> RwLockWriteGuard<'_, HashMap<String, watch::Receiver<GameSnapshot>>> {
        self.live.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A game's registration, held by its task for as long as it is being played.
///
/// **Dropping it deregisters the game.** The game task drops it once the `games`
/// row is inserted — the row is stored, so the next request finds the game
/// in the table rather than in this map — and a task that ends any other way,
/// including by panicking, deregisters on the way out.
#[derive(Debug)]
pub struct Live {
    registry: Arc<Registry>,
    sender: watch::Sender<GameSnapshot>,
    game_id: String,
}

impl Live {
    /// Publishes the game's latest state.
    ///
    /// **Never blocks and never fails.** `watch` keeps one slot: this
    /// overwrites it and wakes whoever is watching, whether or not anybody read
    /// the last one. A send with no receivers left is not an error here — a
    /// game with no spectators is the ordinary case.
    pub fn publish(&self, snapshot: GameSnapshot) {
        self.sender.send_replace(snapshot);
    }

    /// The game this registration is for.
    pub fn game_id(&self) -> &str {
        &self.game_id
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.registry.deregister(&self.game_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::game::{Move, Position, Square};

    /// A snapshot of a game at `START`: hirate, nothing played.
    fn snapshot(game_id: &str, started_at: &str) -> GameSnapshot {
        GameSnapshot {
            game_id: game_id.to_owned(),
            black_name: "engine-a".to_owned(),
            white_name: "engine-b".to_owned(),
            started_at: started_at.to_owned(),
            ply: 0,
            position: Position::hirate(),
            last_move: None,
            clocks: [Duration::from_secs(600); 2],
        }
    }

    /// Hirate after `+7776FU`, so that a published snapshot differs from the
    /// registered one in the position as well as in the count.
    fn advanced(game_id: &str, started_at: &str) -> GameSnapshot {
        let mv = Move::Board {
            from: Square::new(7, 7).expect("7g is on the board"),
            to: Square::new(7, 6).expect("7f is on the board"),
            promote: false,
        };
        let position =
            crate::game::apply_move(&Position::hirate(), mv).expect("7g7f is legal from hirate");

        GameSnapshot {
            ply: 1,
            position,
            last_move: Some("+7776FU,T1".to_owned()),
            ..snapshot(game_id, started_at)
        }
    }

    #[test]
    fn a_registered_game_is_readable_and_a_published_one_replaces_it() {
        let registry = Arc::new(Registry::new());

        let live = registry.register(snapshot("20260819-tabia-1-0", "2026-08-19T12:00:00Z"));

        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.get("20260819-tabia-1-0").map(|game| game.ply),
            Some(0)
        );

        live.publish(advanced("20260819-tabia-1-0", "2026-08-19T12:00:00Z"));

        let read = registry
            .get("20260819-tabia-1-0")
            .expect("the game is still live");
        assert_eq!(read.ply, 1);
        assert_eq!(read.last_move.as_deref(), Some("+7776FU,T1"));
        assert_ne!(read.position, Position::hirate());
    }

    #[test]
    fn dropping_the_handle_deregisters_the_game() {
        let registry = Arc::new(Registry::new());
        let live = registry.register(snapshot("20260819-tabia-1-0", "2026-08-19T12:00:00Z"));

        drop(live);

        assert!(registry.is_empty());
        assert_eq!(registry.get("20260819-tabia-1-0"), None);
    }

    #[test]
    fn a_game_that_is_not_live_is_simply_absent() {
        let registry = Arc::new(Registry::new());

        assert_eq!(registry.get("20260819-tabia-9-9"), None);
        assert_eq!(registry.live(), []);
    }

    #[test]
    fn the_live_list_is_newest_start_first() {
        let registry = Arc::new(Registry::new());
        let handles: Vec<Live> = [
            ("20260819-tabia-1-0", "2026-08-19T12:00:00Z"),
            ("20260819-tabia-1-1", "2026-08-19T13:00:00Z"),
            ("20260819-tabia-1-2", "2026-08-19T11:00:00Z"),
        ]
        .into_iter()
        .map(|(id, started)| registry.register(snapshot(id, started)))
        .collect();

        let ids: Vec<String> = registry
            .live()
            .into_iter()
            .map(|game| game.game_id)
            .collect();

        assert_eq!(
            ids,
            [
                "20260819-tabia-1-1",
                "20260819-tabia-1-0",
                "20260819-tabia-1-2"
            ]
        );

        drop(handles);
        assert!(registry.is_empty());
    }

    #[test]
    fn publishing_with_nobody_watching_is_not_an_error() {
        // A game with no spectators is the ordinary case, and the relay path
        // must not learn to care about that.
        let registry = Arc::new(Registry::new());
        let live = registry.register(snapshot("20260819-tabia-1-0", "2026-08-19T12:00:00Z"));

        for ply in 1..=3 {
            live.publish(GameSnapshot {
                ply,
                ..snapshot("20260819-tabia-1-0", "2026-08-19T12:00:00Z")
            });
        }

        assert_eq!(
            registry.get("20260819-tabia-1-0").map(|game| game.ply),
            Some(3)
        );
    }
}
