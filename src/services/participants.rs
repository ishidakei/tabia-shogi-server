//! Who has played here, and what each of them has played.
//!
//! A participant is an engine, and its public identifier is the token key: a
//! different token is a different engine, and the display name is the engine
//! name most recently used to log in with that token. The key is an identity
//! and not a credential — a digest of a token, from which no token can be
//! recovered.
//!
//! The participant list is what the `games` table has seen, so a token issued
//! and never played is not on it: it has no engine name and no game.
//!
//! The display name is read off the games, which answers for both
//! authentication modes at once: in `open` mode there is no `tokens` row at
//! all.
//!
//! A GitHub identity reaches a page only through [`PublicProfile`]. The path
//! is token key → the `tokens` row that owns it → [`Profiles::profile`] with
//! the viewer of this request, and every step can end in nothing: a
//! participant with no `tokens` row is `open` mode, a `tokens` row whose
//! account has never signed in has no row to filter, and an account that has
//! not published its profile filters to nothing. All three render the same
//! way, with no identity block.
//!
//! A rating comes from the [`Ratings`] view the token list already reads, so
//! `None` — rendered as unrated — is the true answer for a participant below
//! the rated threshold.

use std::sync::Arc;

use crate::storage::{Database, ParticipantRow, Tokens, token_hash};

use super::games::{self, FinishedEntry, PAGE};
use super::privacy::{AccountId, Profiles, PublicProfile};
use super::rating::Ratings;

/// One participant, as the list shows it.
///
/// Private fields and accessors, so what a template may read is what this
/// module chose to lend it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticipantEntry {
    token_key: String,
    display_name: String,
    rating: Option<i32>,
    games_played: u64,
    last_ended_at: String,
}

impl ParticipantEntry {
    /// The public identifier, and the last segment of this participant's page.
    pub fn token_key(&self) -> &str {
        &self.token_key
    }

    /// The engine name most recently used with this token.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// What the ratings view rates it — `None` for a participant no published
    /// table rates.
    pub const fn rating(&self) -> Option<i32> {
        self.rating
    }

    /// How many finished games it has played, on either side.
    pub const fn games_played(&self) -> u64 {
        self.games_played
    }

    /// When its newest game ended, RFC 3339 in UTC.
    pub fn last_ended_at(&self) -> &str {
        &self.last_ended_at
    }
}

/// One participant's page: who they are, and a page of what they have played.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Participant {
    token_key: String,
    display_name: String,
    rating: Option<i32>,
    identity: Option<PublicProfile>,
    games_played: u64,
    last_ended_at: String,
    games: Vec<FinishedEntry>,
    older: Option<String>,
}

impl Participant {
    /// The public identifier.
    pub fn token_key(&self) -> &str {
        &self.token_key
    }

    /// The engine name most recently used with this token.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// What the ratings view rates it.
    pub const fn rating(&self) -> Option<i32> {
        self.rating
    }

    /// The owner's GitHub identity as this viewer may see it, or `None` when
    /// there is no identity block to render.
    ///
    /// The three ways to reach `None` are one answer: a page that told them
    /// apart would be answering a question about the owner that the owner did
    /// not publish. `Some` is the whole profile.
    pub const fn identity(&self) -> Option<&PublicProfile> {
        self.identity.as_ref()
    }

    /// How many finished games this participant has played in total — not the
    /// number on this page.
    pub const fn games_played(&self) -> u64 {
        self.games_played
    }

    /// When its newest game ended — the participant's, not this page's.
    pub fn last_ended_at(&self) -> &str {
        &self.last_ended_at
    }

    /// This page of its games, newest end first.
    pub fn games(&self) -> &[FinishedEntry] {
        &self.games
    }

    /// The cursor for the page of older games, or `None` at the end of them.
    pub fn older(&self) -> Option<&str> {
        self.older.as_deref()
    }
}

/// The participant-side half of the web service layer.
///
/// The [`Ratings`] view is the same value the token store reads, handed over
/// rather than rebuilt, so a token and the participant it is cannot show two
/// different figures.
#[derive(Clone, Debug)]
pub struct Participants {
    database: Arc<Database>,
    tokens: Tokens,
    profiles: Profiles,
    ratings: Arc<dyn Ratings>,
}

impl Participants {
    /// The stores a participant is assembled from, and how a rating is read.
    ///
    /// The `tokens` handle is built here from the database already passed in,
    /// so the shutdown that closes the database closes this too.
    pub fn new(database: Arc<Database>, profiles: Profiles, ratings: Arc<dyn Ratings>) -> Self {
        let tokens = Tokens::of(&database);

        Self {
            database,
            tokens,
            profiles,
            ratings,
        }
    }

    /// Every participant this server has seen, newest game first.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn listing(&self) -> Result<Vec<ParticipantEntry>, sqlx::Error> {
        Ok(self
            .database
            .participants()
            .await?
            .into_iter()
            .map(|row| self.entry(row))
            .collect())
    }

    /// One participant's page, or `None` for a key that has played nothing.
    ///
    /// `key` is the last URL segment, so it is client input, and the first
    /// thing done with it is [`token_hash`]: text that is not a key this
    /// server could have written decodes to nothing. What reaches a query is
    /// the key re-encoded from that digest.
    ///
    /// `viewer` is the signed-in account, and `None` for a spectator.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said, on any of the three tables a page reads.
    pub async fn of(
        &self,
        key: &str,
        viewer: Option<AccountId>,
        before: Option<&str>,
    ) -> Result<Option<Participant>, sqlx::Error> {
        let Some(hash) = token_hash(key) else {
            return Ok(None);
        };
        // The summary before the page of games, because it is what decides
        // whether there is a participant at all: an `?before=` cursor past the
        // oldest game is an empty page rather than a 404.
        let Some(row) = self.database.participant(&hash).await? else {
            return Ok(None);
        };

        let played = self.database.games_of(&hash, before, PAGE).await?;
        // Only ever `Some` when this page filled: a short page has reached the
        // end.
        let filled = u32::try_from(played.len()).unwrap_or(u32::MAX) >= PAGE;
        let older = filled
            .then(|| played.last().map(|game| game.ended_at.clone()))
            .flatten();

        // The privacy filter, with the viewer of this request. `None` at any
        // step is no identity block.
        let identity = match self.tokens.account_of(&hash).await? {
            Some(account) => self.profiles.profile(account, viewer).await?,
            None => None,
        };

        let entry = self.entry(row);

        Ok(Some(Participant {
            token_key: entry.token_key,
            display_name: entry.display_name,
            rating: entry.rating,
            identity,
            games_played: entry.games_played,
            last_ended_at: entry.last_ended_at,
            games: played.iter().map(games::finished_entry).collect(),
            older,
        }))
    }

    /// A derived row as the list shows it, with the view's rating on it.
    ///
    /// The one place a [`ParticipantEntry`] is built, and the page above is
    /// built from one too, so the list's figure and the page's are the same
    /// figure by construction.
    fn entry(&self, row: ParticipantRow) -> ParticipantEntry {
        ParticipantEntry {
            rating: self.ratings.rating_of(&row.token_key),
            token_key: row.token_key,
            display_name: row.display_name,
            games_played: row.games,
            last_ended_at: row.last_ended_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::auth::token;
    use crate::services::rating::Unrated;
    use crate::services::rating::tests::SeededRatings;
    use crate::storage::testing::temp_dir;
    use crate::storage::{
        Accounts, Caps, GameRow, Records, StartCategory, TimeCategory, Winner, token_key,
    };

    const ALICE: AccountId = 4_242;
    const BOB: AccountId = 9_001;

    const NAME: &str = "alice";
    const AVATAR: &str = "https://avatars.example/alice.png";

    /// The default caps.
    const DEFAULTS: Caps = Caps {
        active: 3,
        lifetime: 16,
    };

    /// A moment, in the column's convention.
    const AT: &str = "2026-08-27T09:00:00Z";

    /// The key of the token named after `seed`.
    fn key_of(seed: &str) -> String {
        token_key(&token::hash(seed))
    }

    /// A finished game between two token keys, under two engine names.
    fn game(game_id: &str, ended_at: &str, black: (&str, &str), white: (&str, &str)) -> GameRow {
        GameRow {
            game_id: game_id.to_owned(),
            black_name: black.0.to_owned(),
            white_name: white.0.to_owned(),
            black_token_key: key_of(black.1),
            white_token_key: key_of(white.1),
            start_category: StartCategory::Designated,
            time_category: TimeCategory::Symmetric,
            started_at: "2026-08-27T11:00:00Z".to_owned(),
            ended_at: ended_at.to_owned(),
            end_status: "RESIGN".to_owned(),
            result: Winner::Black,
            ply_count: 41,
            record_path: Records::relative_path(game_id),
            start_position: Some("position startpos moves 7g7f 3c3d".to_owned()),
        }
    }

    /// A fresh database, and everything a participant is assembled from.
    async fn fresh(
        name: &str,
        ratings: Arc<dyn Ratings>,
    ) -> (PathBuf, Arc<Database>, Participants) {
        let dir = temp_dir(&format!("services-participants-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Arc::new(
            Database::open(dir.join("tabia.sqlite3"))
                .await
                .expect("a fresh file opens"),
        );
        let profiles = Profiles::new(Accounts::of(&database));
        let participants = Participants::new(Arc::clone(&database), profiles, ratings);

        (dir, database, participants)
    }

    /// The same with nothing rated: the empty view, which is what a server that
    /// has published no table answers.
    async fn unrated(name: &str) -> (PathBuf, Arc<Database>, Participants) {
        fresh(name, Arc::new(Unrated)).await
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_list_is_one_entry_per_token_key_named_by_its_newest_game() {
        // The display-name rule: the engine name most recently used with the
        // token. One token plays under two names, and the newer one is what the
        // list shows.
        let (dir, database, participants) = unrated("list").await;
        for row in [
            game(
                "20260827-tabia-1-0",
                "2026-08-27T12:00:00Z",
                ("engine-c", "token-c"),
                ("engine-d", "token-d"),
            ),
            game(
                "20260827-tabia-1-1",
                "2026-08-27T13:00:00Z",
                ("engine-a", "token-a"),
                ("engine-b", "token-b"),
            ),
            game(
                "20260827-tabia-1-2",
                "2026-08-27T14:00:00Z",
                ("engine-b", "token-b"),
                ("engine-a-v2", "token-a"),
            ),
        ] {
            database.insert_game(&row).await.expect("it inserts");
        }

        let listed = participants.listing().await.expect("selectable");

        // Four participants for four token keys, not six for six appearances.
        assert_eq!(listed.len(), 4, "{listed:?}");

        let played_twice = listed
            .iter()
            .find(|entry| entry.token_key() == key_of("token-a"))
            .expect("the participant that played twice is listed");
        // The name is the newest game's, and that game was played as White: the
        // rule is "most recently used with this token", not "as Black".
        assert_eq!(played_twice.display_name(), "engine-a-v2");
        // Counted once per game and not once per column.
        assert_eq!(played_twice.games_played(), 2);
        assert_eq!(played_twice.last_ended_at(), "2026-08-27T14:00:00Z");
        // The empty view rates nobody, which is what a server that has
        // published no table answers.
        assert_eq!(played_twice.rating(), None);

        // Newest game first: the two who played at 14:00 come before the two
        // who last played at 12:00. Within one moment the key orders them,
        // which is what keeps the list stable rather than meaningful.
        let order: Vec<&str> = listed.iter().map(ParticipantEntry::last_ended_at).collect();
        assert_eq!(
            order,
            [
                "2026-08-27T14:00:00Z",
                "2026-08-27T14:00:00Z",
                "2026-08-27T12:00:00Z",
                "2026-08-27T12:00:00Z"
            ]
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_empty_server_has_no_participants_rather_than_an_error() {
        let (dir, _database, participants) = unrated("empty").await;

        assert_eq!(participants.listing().await.expect("selectable"), []);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_page_is_that_participants_games_newest_first() {
        let (dir, database, participants) = unrated("page").await;
        for (seq, ended) in [(0, "2026-08-27T12:00:00Z"), (1, "2026-08-27T13:00:00Z")] {
            let row = game(
                &format!("20260827-tabia-1-{seq}"),
                ended,
                ("engine-a", "token-a"),
                ("engine-b", "token-b"),
            );
            database.insert_game(&row).await.expect("it inserts");
        }
        // A game the participant is not in, which must not be on their page.
        database
            .insert_game(&game(
                "20260827-tabia-2-0",
                "2026-08-27T14:00:00Z",
                ("engine-c", "token-c"),
                ("engine-d", "token-d"),
            ))
            .await
            .expect("it inserts");

        let page = participants
            .of(&key_of("token-a"), None, None)
            .await
            .expect("selectable")
            .expect("the participant has games");

        assert_eq!(page.display_name(), "engine-a");
        assert_eq!(page.games_played(), 2);
        assert_eq!(page.rating(), None);
        let ids: Vec<&str> = page.games().iter().map(FinishedEntry::game_id).collect();
        assert_eq!(ids, ["20260827-tabia-1-1", "20260827-tabia-1-0"]);
        // One page held everything, so there is nothing older to offer.
        assert_eq!(page.older(), None);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_cursor_walks_back_through_one_participants_games() {
        let (dir, database, participants) = unrated("cursor").await;
        for (seq, ended) in [(0, "2026-08-27T12:00:00Z"), (1, "2026-08-27T13:00:00Z")] {
            let row = game(
                &format!("20260827-tabia-1-{seq}"),
                ended,
                ("engine-a", "token-a"),
                ("engine-b", "token-b"),
            );
            database.insert_game(&row).await.expect("it inserts");
        }

        let older = participants
            .of(&key_of("token-a"), None, Some("2026-08-27T13:00:00Z"))
            .await
            .expect("selectable")
            .expect("the participant is still there");

        let ids: Vec<&str> = older.games().iter().map(FinishedEntry::game_id).collect();
        assert_eq!(ids, ["20260827-tabia-1-0"]);
        // The summary is the participant's, not the page's: a cursor narrows
        // which games are shown and not how many they have played.
        assert_eq!(older.games_played(), 2);

        // And a cursor past the oldest game is an empty page of a real
        // participant rather than a 404.
        let past = participants
            .of(&key_of("token-a"), None, Some("2026-08-01T00:00:00Z"))
            .await
            .expect("selectable")
            .expect("the participant is still there");
        assert_eq!(past.games(), []);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_key_that_played_nothing_and_text_that_is_no_key_are_one_answer() {
        // Which identifiers exist is not something a URL tells anybody: an
        // unplayed key, a key of the right shape that was never issued, and text
        // that is not a key at all are the same `None`.
        let (dir, database, participants) = unrated("unknown").await;
        database
            .insert_game(&game(
                "20260827-tabia-1-0",
                "2026-08-27T12:00:00Z",
                ("engine-a", "token-a"),
                ("engine-b", "token-b"),
            ))
            .await
            .expect("it inserts");

        for key in [
            key_of("never-played"),
            "not-a-key".to_owned(),
            String::new(),
            key_of("token-a").to_uppercase(),
        ] {
            assert_eq!(
                participants.of(&key, None, None).await.expect("selectable"),
                None,
                "{key}"
            );
        }

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_open_mode_participant_has_no_tokens_row_and_no_identity() {
        // `open` mode writes no `tokens` row, so the account lookup finds
        // nothing and the page has no identity block. No mode is consulted:
        // the absence of the row is the whole of it.
        let (dir, database, participants) = unrated("open").await;
        database
            .insert_game(&game(
                "20260827-tabia-1-0",
                "2026-08-27T12:00:00Z",
                ("engine-a", "token-a"),
                ("engine-b", "token-b"),
            ))
            .await
            .expect("it inserts");

        let page = participants
            .of(&key_of("token-a"), None, None)
            .await
            .expect("selectable")
            .expect("the participant has games");

        assert_eq!(page.identity(), None);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    /// A `github`-mode token of `ALICE`'s, played into one finished game.
    ///
    /// The three writes a real `github`-mode game leaves behind it: the account
    /// row a sign-in makes, the `tokens` row the issue page makes, and the
    /// `games` row the game itself makes. Nothing here is a fixture standing in
    /// for one of them — each is written through the store that owns it.
    async fn alices_game(database: &Database) {
        let (_token, hash) = token::generate();
        Accounts::of(database)
            .sign_in(ALICE, NAME, AVATAR)
            .await
            .expect("it inserts");
        Tokens::of(database)
            .issue(ALICE, &hash, None, Some(DEFAULTS), AT)
            .await
            .expect("the account is under both caps");

        let mut row = game(
            "20260827-tabia-1-0",
            "2026-08-27T12:00:00Z",
            ("engine-a", "unused"),
            ("engine-b", "token-b"),
        );
        // The game is filed under the same digest the `tokens` row holds, which
        // is what makes the two rows one participant.
        row.black_token_key = token_key(&hash);
        database.insert_game(&row).await.expect("it inserts");
    }

    /// The key `alices_game` filed its game under.
    async fn alices_key(participants: &Participants) -> String {
        participants
            .listing()
            .await
            .expect("selectable")
            .into_iter()
            .find(|entry| entry.display_name() == "engine-a")
            .expect("alice's game is listed")
            .token_key()
            .to_owned()
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_fresh_accounts_participant_shows_no_identity_to_anybody_but_its_owner() {
        // The privacy default, seen from a participant page: the owner's own
        // three fields are there for the owner, and every other viewer — another
        // account, and nobody —
        // gets no identity block rather than a blank one.
        let (dir, database, participants) = unrated("fresh-account").await;
        alices_game(&database).await;
        let key = alices_key(&participants).await;

        for viewer in [Some(BOB), None] {
            let page = participants
                .of(&key, viewer, None)
                .await
                .expect("selectable")
                .expect("the participant has games");

            assert_eq!(page.identity(), None, "{viewer:?}");
        }

        let owner = participants
            .of(&key, Some(ALICE), None)
            .await
            .expect("selectable")
            .expect("the participant has games")
            .identity()
            .cloned()
            .expect("the owner sees their own account");
        assert_eq!(owner.account_name(), NAME);
        assert_eq!(owner.avatar_url(), AVATAR);
        assert_eq!(owner.account_id(), ALICE);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_published_profile_is_the_whole_profile_a_stranger_sees() {
        // The filter, at the first public page that reads it: the switch moves,
        // and all three items appear — to a signed-in stranger and to nobody
        // alike.
        let (dir, database, participants) = unrated("published").await;
        alices_game(&database).await;
        let key = alices_key(&participants).await;
        let profiles = Profiles::new(Accounts::of(&database));
        profiles
            .set_visibility(ALICE, true)
            .await
            .expect("the store answers");

        for viewer in [Some(BOB), None] {
            let identity = participants
                .of(&key, viewer, None)
                .await
                .expect("selectable")
                .expect("the participant has games")
                .identity()
                .cloned()
                .expect("the profile is published");

            assert_eq!(identity.account_name(), NAME, "{viewer:?}");
            assert_eq!(identity.avatar_url(), AVATAR, "{viewer:?}");
            assert_eq!(identity.account_id(), ALICE, "{viewer:?}");
        }

        // And taking it back is the identity block going away again, at the
        // next read and with nothing invalidated.
        profiles
            .set_visibility(ALICE, false)
            .await
            .expect("the store answers");
        assert_eq!(
            participants
                .of(&key, None, None)
                .await
                .expect("selectable")
                .expect("the participant has games")
                .identity(),
            None
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_token_whose_owner_has_never_signed_in_has_no_identity_either() {
        // A `tokens` row exists and the `accounts` row does not, which is the
        // state a sign-in fills in. It renders as `open` mode does.
        let (dir, database, participants) = unrated("no-account").await;
        let (_token, hash) = token::generate();
        Tokens::of(&database)
            .issue(ALICE, &hash, None, Some(DEFAULTS), AT)
            .await
            .expect("the account is under both caps");
        let mut row = game(
            "20260827-tabia-1-0",
            "2026-08-27T12:00:00Z",
            ("engine-a", "unused"),
            ("engine-b", "token-b"),
        );
        row.black_token_key = token_key(&hash);
        database.insert_game(&row).await.expect("it inserts");

        let key = alices_key(&participants).await;

        assert_eq!(
            participants
                .of(&key, Some(ALICE), None)
                .await
                .expect("selectable")
                .expect("the participant has games")
                .identity(),
            None
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_rating_is_what_the_view_says_about_the_token_key() {
        // The ratings view, asked the way it is asked: by the
        // key the games are filed under, on the list and on the page alike.
        let (dir, database, _unrated) = unrated("rated").await;
        let rated = key_of("token-a");
        let participants = Participants::new(
            Arc::clone(&database),
            Profiles::new(Accounts::of(&database)),
            Arc::new(SeededRatings::of([(rated.clone(), 2_150)])),
        );
        database
            .insert_game(&game(
                "20260827-tabia-1-0",
                "2026-08-27T12:00:00Z",
                ("engine-a", "token-a"),
                ("engine-b", "token-b"),
            ))
            .await
            .expect("it inserts");

        let listed = participants.listing().await.expect("selectable");
        let entry = listed
            .iter()
            .find(|entry| entry.token_key() == rated)
            .expect("the rated participant is listed");
        assert_eq!(entry.rating(), Some(2_150));
        // The unrated opponent is on the same list with no figure.
        assert_eq!(
            listed
                .iter()
                .find(|entry| entry.token_key() == key_of("token-b"))
                .and_then(ParticipantEntry::rating),
            None
        );

        assert_eq!(
            participants
                .of(&rated, None, None)
                .await
                .expect("selectable")
                .expect("the participant has games")
                .rating(),
            Some(2_150)
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
