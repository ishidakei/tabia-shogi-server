//! The admin page's logic: who administers this instance, and what they may
//! designate.
//!
//! An administrator sets the designated rating of an engine that is not a
//! preset. Nothing here can revoke a token, end a game, or read an account.
//!
//! [`administers`](Administration::administers) is read on one other route,
//! where it grants no page: an account this answers `true` for issues tokens
//! with `[accounts]`'s two caps waived.
//!
//! An external engine's designated rating is a row rather than a configuration
//! key, because a key would mean editing a file on the host and restarting the
//! server to change one, while the rating job reads the table on every run. A
//! preset's designated rating is on the entry that registers it instead, and
//! this module refuses to write one.
//!
//! Presets are excluded twice: a preset's participant ID is absent from the
//! engines the page offers and refused on submission. The list is a
//! convenience; the refusal is the rule.
//!
//! An administrator never handles a participant ID. The page is two tables of
//! rows the server built, and each row submits its own identity from a hidden
//! field, so the value the page offers to type is a rating: a number sets or
//! changes the designation, and an empty rating removes it.
//!
//! A refusal carries no text the request supplied. A submission that names an
//! engine by something other than a participant ID can only be hand-crafted,
//! and a token pasted into one is a credential an echoing refusal would render
//! into HTML and write into the handler's log.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::storage::{Database, Designations, is_token_key};

use super::privacy::AccountId;

/// The administrators, the presets they may not designate, and the store.
///
/// One value rather than three parameters, since a page that took them apart
/// could be given the administrators of one configuration and the presets of
/// another.
///
/// The two configured halves are participant IDs and account ids, never
/// tokens: the preset tokens are hashed where the configuration is read.
#[derive(Clone, Debug)]
pub struct Administration {
    administrators: Vec<AccountId>,
    presets: BTreeSet<String>,
    designations: Designations,
    database: Arc<Database>,
}

impl Administration {
    /// The administration view of a running server.
    ///
    /// `administrators` is `[web].administrators` as written, and `presets`
    /// the participant ID of every registered preset engine — designated or
    /// not, because the exclusion is about being a preset.
    pub fn new(
        administrators: Vec<AccountId>,
        presets: BTreeSet<String>,
        database: Arc<Database>,
    ) -> Self {
        let designations = Designations::of(&database);

        Self {
            administrators,
            presets,
            designations,
            database,
        }
    }

    /// Whether `account` administers this instance.
    ///
    /// A membership test on the id the session already carries: no query, and
    /// nothing that can fail. An empty list administers nothing.
    pub fn administers(&self, account: AccountId) -> bool {
        self.administrators.contains(&account)
    }

    /// What the admin page shows: the current designations, and the engines
    /// that could be designated.
    ///
    /// Two reads, on a page only an administrator reaches. The participants
    /// are read for the engine name beside a designation and for the second
    /// table at once, so an administrator reads a name rather than a digest.
    ///
    /// **A designation of an engine that has finished no game is still shown**,
    /// with no name. Nothing the page offers can create one — an engine is
    /// designated from a row of the second table, which is built from finished
    /// games — but the store accepts one, so a hand-crafted submission can
    /// leave a row here, and a page that dropped it would be a page an
    /// administrator cannot use to remove it.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said, on either read.
    pub async fn page(&self) -> Result<DesignationsPage, sqlx::Error> {
        let designated = self.designations.all().await?;
        let participants = self.database.participants().await?;

        let entries = designated
            .iter()
            .map(|row| DesignationEntry {
                display_name: participants
                    .iter()
                    .find(|participant| participant.token_key == row.token_key)
                    .map(|participant| participant.display_name.clone()),
                token_key: row.token_key.clone(),
                rating: row.rating,
                designated_by: row.designated_by,
                designated_at: row.designated_at.clone(),
            })
            .collect();

        let candidates = participants
            .into_iter()
            .filter(|participant| !self.presets.contains(&participant.token_key))
            .filter(|participant| {
                !designated
                    .iter()
                    .any(|row| row.token_key == participant.token_key)
            })
            .map(|participant| Candidate {
                token_key: participant.token_key,
                display_name: participant.display_name,
            })
            .collect();

        Ok(DesignationsPage {
            entries,
            candidates,
        })
    }

    /// Designates `rating` for `participant`, removes the designation if
    /// `rating` is empty, or says why not.
    ///
    /// `participant` and `rating` are the form's fields verbatim: the whole
    /// rule is applied here, where the presets are known.
    ///
    /// A designation that names an engine already designated replaces its
    /// value. An empty rating is a removal, and the only one; whitespace is
    /// empty too, because a field cleared with the space bar is a field an
    /// administrator considers cleared.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said. A refusal is a [`Designating::Refused`], not an
    /// error, and the page names the remedy.
    pub async fn designate(
        &self,
        by: AccountId,
        participant: &str,
        rating: &str,
        at: &str,
    ) -> Result<Designating, sqlx::Error> {
        if !is_token_key(participant) {
            return Ok(Designating::Refused(DesignationRefusal::NotAParticipantId));
        }

        // Before the number is looked at, so that a page never reports "not a
        // number" for a preset's ID.
        if self.presets.contains(participant) {
            return Ok(Designating::Refused(DesignationRefusal::PresetEngine));
        }

        let rating = rating.trim();
        if rating.is_empty() {
            let removed = self.designations.remove(participant).await?;

            return Ok(Designating::Removed { removed });
        }

        let Ok(rating) = rating.parse::<i32>() else {
            return Ok(Designating::Refused(DesignationRefusal::RatingNotANumber));
        };

        self.designations.set(participant, rating, by, at).await?;

        Ok(Designating::Set { rating })
    }
}

/// What the admin page renders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesignationsPage {
    entries: Vec<DesignationEntry>,
    candidates: Vec<Candidate>,
}

impl DesignationsPage {
    /// The current designations, by ascending participant ID.
    pub fn entries(&self) -> &[DesignationEntry] {
        &self.entries
    }

    /// The engines an administrator may designate that are not designated yet:
    /// every participant this server has seen, minus the presets.
    ///
    /// One table row each, and the row carries the identity, which is why an
    /// engine that has finished no game cannot be designated at all: it is not
    /// a participant, so it has no row.
    ///
    /// Newest game first, the order the public participant list is in.
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }
}

/// One designation, as the page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesignationEntry {
    token_key: String,
    display_name: Option<String>,
    rating: i32,
    designated_by: AccountId,
    designated_at: String,
}

impl DesignationEntry {
    /// The engine's participant ID, and the last segment of its page's URL.
    pub fn token_key(&self) -> &str {
        &self.token_key
    }

    /// The engine name of its newest game, or `None` for an engine that has
    /// finished none.
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// The designated rating in force.
    pub const fn rating(&self) -> i32 {
        self.rating
    }

    /// The GitHub user id of the administrator who set it.
    ///
    /// An id rather than a name, because that is what `[web].administrators`
    /// lists and what the row stores; looking a name up would be a github.com
    /// request made to decorate a table.
    pub const fn designated_by(&self) -> AccountId {
        self.designated_by
    }

    /// When it was set, RFC 3339 in UTC.
    pub fn designated_at(&self) -> &str {
        &self.designated_at
    }
}

/// One engine an administrator may designate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    token_key: String,
    display_name: String,
}

impl Candidate {
    /// The engine's participant ID — what the row's hidden field submits, so
    /// that nobody types it.
    pub fn token_key(&self) -> &str {
        &self.token_key
    }

    /// The engine name of its newest game, which is what an administrator picks
    /// a row by.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

/// What a designation attempt did.
///
/// A refusal is an ordinary answer to an ordinary request, so it travels
/// beside the success rather than as an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Designating {
    /// The engine is designated at this value, whether or not it was before.
    Set {
        /// The value now in force.
        rating: i32,
    },

    /// The engine has no designated rating, which an empty rating asked for.
    Removed {
        /// Whether this call is what removed one.
        ///
        /// `false` is "there was nothing to remove". The state the
        /// administrator asked for holds either way, so this only tells the
        /// log line which of the two happened.
        removed: bool,
    },

    /// Nothing was written, for this reason.
    Refused(DesignationRefusal),
}

/// Why a designation was not written.
///
/// No variant carries what the request sent: a submission the page could not
/// have produced carries unknown text, which may be a credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesignationRefusal {
    /// The engine was not named by a participant ID.
    NotAParticipantId,

    /// The participant ID is a preset engine's.
    PresetEngine,

    /// The rating was not a number.
    RatingNotANumber,
}

impl DesignationRefusal {
    /// The reason as a fixed word, for the log line the handler writes.
    ///
    /// A constant, never the text that caused it.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotAParticipantId => "not a participant id",
            Self::PresetEngine => "a preset engine",
            Self::RatingNotANumber => "a rating that is not a number",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::auth::token;
    use crate::storage::testing::temp_dir;
    use crate::storage::{GameRow, StartCategory, TimeCategory, Winner, token_key};

    /// Two administrators and one account that is not one.
    const ALICE: AccountId = 4_242;
    const BOB: AccountId = 9_001;
    const STRANGER: AccountId = 7;

    /// The moment every write below is stamped with.
    const NOW: &str = "2026-08-31T12:00:00Z";

    /// The participant ID of the engine that logs in with `token`.
    fn key_of(token: &str) -> String {
        token_key(&token::hash(token))
    }

    /// A fresh database and an administration over it.
    ///
    /// Two administrators and one preset: a membership test that can fail, and
    /// an engine that may not be designated here.
    async fn fresh(name: &str) -> (PathBuf, Arc<Database>, Administration) {
        let dir = temp_dir(&format!("administration-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Arc::new(
            Database::open(dir.join("tabia.sqlite3"))
                .await
                .expect("a fresh file opens"),
        );
        let administration = Administration::new(
            vec![ALICE, BOB],
            BTreeSet::from([key_of("preset-token")]),
            Arc::clone(&database),
        );

        (dir, database, administration)
    }

    /// Files one finished game between two engines, so that both are
    /// participants.
    async fn played(database: &Database, black: &str, white: &str, game_id: &str) {
        database
            .insert_game(&GameRow {
                game_id: game_id.to_owned(),
                black_name: black.to_owned(),
                white_name: white.to_owned(),
                black_token_key: key_of(black),
                white_token_key: key_of(white),
                start_category: StartCategory::Designated,
                time_category: TimeCategory::Symmetric,
                started_at: "2026-08-31T11:00:00Z".to_owned(),
                ended_at: "2026-08-31T11:30:00Z".to_owned(),
                end_status: "RESIGN".to_owned(),
                result: Winner::Black,
                ply_count: 43,
                record_path: format!("{game_id}.csa"),
                start_position: Some("position startpos moves 7g7f 3c3d".to_owned()),
            })
            .await
            .expect("the insert runs");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_administrator_is_exactly_an_account_the_configuration_lists() {
        let (dir, database, administration) = fresh("membership").await;

        assert!(administration.administers(ALICE));
        assert!(administration.administers(BOB));
        assert!(!administration.administers(STRANGER));

        // The shipped configuration lists nobody.
        let nobody = Administration::new(Vec::new(), BTreeSet::new(), Arc::clone(&database));
        assert!(!nobody.administers(ALICE));

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_designation_is_written_read_back_and_replaced() {
        let (dir, database, administration) = fresh("write").await;
        let engine = key_of("engine-a");
        played(&database, "engine-a", "engine-b", "20260831-tabia-1-1").await;

        assert_eq!(
            administration
                .designate(ALICE, &engine, "2400", NOW)
                .await
                .expect("the write runs"),
            Designating::Set { rating: 2_400 }
        );

        let page = administration.page().await.expect("the reads run");
        let entry = page.entries().first().expect("one designation");
        assert_eq!(entry.token_key(), engine);
        assert_eq!(entry.rating(), 2_400);
        assert_eq!(entry.designated_by(), ALICE);
        assert_eq!(entry.designated_at(), NOW);
        // The name comes from the engine's newest game.
        assert_eq!(entry.display_name(), Some("engine-a"));

        // Designating it again is a change rather than a second row, and a
        // negative value is a scale an operator is free to choose.
        administration
            .designate(BOB, &engine, " -100 ", "2026-08-31T13:00:00Z")
            .await
            .expect("the write runs");
        let page = administration.page().await.expect("the reads run");
        assert_eq!(page.entries().len(), 1);
        assert_eq!(page.entries()[0].rating(), -100);
        assert_eq!(page.entries()[0].designated_by(), BOB);

        // Saving the empty field again removes nothing and is not an error:
        // the state it asks for already holds.
        assert_eq!(
            administration
                .designate(ALICE, &engine, "", "2026-08-31T14:00:00Z")
                .await
                .expect("the delete runs"),
            Designating::Removed { removed: true }
        );
        assert_eq!(
            administration
                // Whitespace, because a field cleared with the space bar is a
                // field an administrator considers cleared.
                .designate(ALICE, &engine, "  ", "2026-08-31T15:00:00Z")
                .await
                .expect("the delete runs"),
            Designating::Removed { removed: false }
        );
        let page = administration.page().await.expect("the reads run");
        assert!(page.entries().is_empty());
        // And the engine is offered again, so the removal is undoable from the
        // page it was made on.
        assert!(
            page.candidates()
                .iter()
                .any(|candidate| candidate.token_key() == engine)
        );

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_preset_engine_is_refused_even_though_it_is_a_participant() {
        // A preset's designated rating is the configuration's statement, on the
        // entry that registers it, so the page neither offers one nor accepts
        // one.
        let (dir, database, administration) = fresh("preset").await;
        played(&database, "preset-token", "engine-b", "20260831-tabia-2-1").await;

        assert_eq!(
            administration
                .designate(ALICE, &key_of("preset-token"), "2400", NOW)
                .await
                .expect("the call runs"),
            Designating::Refused(DesignationRefusal::PresetEngine)
        );

        // Nothing was written, and the preset is not among the candidates the
        // page offers — while the engine it played is.
        let page = administration.page().await.expect("the reads run");
        assert!(page.entries().is_empty());
        let offered: Vec<&str> = page
            .candidates()
            .iter()
            .map(|candidate| candidate.token_key())
            .collect();
        assert!(
            !offered.contains(&key_of("preset-token").as_str()),
            "{offered:?}"
        );
        assert!(
            offered.contains(&key_of("engine-b").as_str()),
            "{offered:?}"
        );

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_engine_already_designated_is_not_offered_a_second_time() {
        let (dir, database, administration) = fresh("offered-once").await;
        played(&database, "engine-a", "engine-b", "20260831-tabia-3-1").await;

        administration
            .designate(ALICE, &key_of("engine-a"), "2400", NOW)
            .await
            .expect("the write runs");

        let page = administration.page().await.expect("the reads run");
        let offered: Vec<&str> = page
            .candidates()
            .iter()
            .map(|candidate| candidate.token_key())
            .collect();

        // It is on the page already, with its own input.
        assert_eq!(offered, [key_of("engine-b").as_str()]);

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_malformed_submission_is_refused_and_writes_nothing() {
        let (dir, database, administration) = fresh("refusals").await;

        for written in [
            String::new(),
            "not-a-digest".to_owned(),
            "a".repeat(63),
            "a".repeat(65),
            // Upper case: this server writes a participant ID lower case
            // everywhere.
            "A".repeat(64),
            "g".repeat(64),
        ] {
            assert_eq!(
                administration
                    .designate(ALICE, &written, "2400", NOW)
                    .await
                    .expect("the call runs"),
                Designating::Refused(DesignationRefusal::NotAParticipantId),
                "{written:?}"
            );
            // And the same text with an empty rating — the removal — is refused
            // for the same reason rather than removing nothing quietly.
            assert_eq!(
                administration
                    .designate(ALICE, &written, "", NOW)
                    .await
                    .expect("the call runs"),
                Designating::Refused(DesignationRefusal::NotAParticipantId),
                "{written:?}"
            );
        }

        // An empty rating is a removal rather than a refusal, so what is left
        // here is text that is neither empty nor a number.
        for written in ["2400.5", "2,400", "high", "  2400 x"] {
            assert_eq!(
                administration
                    .designate(ALICE, &key_of("engine-a"), written, NOW)
                    .await
                    .expect("the call runs"),
                Designating::Refused(DesignationRefusal::RatingNotANumber),
                "{written:?}"
            );
        }

        assert!(
            administration
                .page()
                .await
                .expect("the reads run")
                .entries()
                .is_empty()
        );

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_designation_of_an_engine_with_no_game_is_shown_and_can_be_removed() {
        // The page offers no way to make one — an engine is designated from a
        // row built out of finished games — but the store accepts one, so a
        // hand-crafted submission can leave a row here. A page that dropped it
        // would be a page an administrator cannot use to remove it.
        let (dir, database, administration) = fresh("unplayed").await;
        let engine = key_of("never-played");

        administration
            .designate(ALICE, &engine, "1800", NOW)
            .await
            .expect("the write runs");

        let page = administration.page().await.expect("the reads run");
        let entry = page.entries().first().expect("one designation");
        assert_eq!(entry.token_key(), engine);
        assert_eq!(entry.display_name(), None);
        assert!(page.candidates().is_empty());

        assert_eq!(
            administration
                .designate(ALICE, &engine, "", NOW)
                .await
                .expect("the delete runs"),
            Designating::Removed { removed: true }
        );
        assert!(
            administration
                .page()
                .await
                .expect("the reads run")
                .entries()
                .is_empty()
        );

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_refusal_has_a_fixed_word_and_carries_no_value() {
        // What reaches a log line is one of these three, never the text a
        // submission carried: a request the page could not have produced can
        // carry anything, including a credential.
        for refusal in [
            DesignationRefusal::NotAParticipantId,
            DesignationRefusal::PresetEngine,
            DesignationRefusal::RatingNotANumber,
        ] {
            assert!(!refusal.reason().is_empty());
            assert_eq!(format!("{refusal:?}").matches('(').count(), 0);
        }
    }
}
