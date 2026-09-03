//! The privacy filter, and the account page behind it.
//!
//! [`PublicProfile`] is a type boundary, not a template convention: the only
//! way to obtain one is [`PublicProfile::of`], whose second argument is the
//! viewer, so a page that forgets to filter has nothing to render. The filter
//! belongs at the fetch rather than at the render, because a rule applied at
//! render leaks through every other path to the same data.
//!
//! [`Profiles`] is that fetch: it returns a filtered profile or the owner's
//! own [`AccountSettings`], and no method here hands a raw [`AccountRow`] to
//! the layer above. The owner's page goes through the same filter, with the
//! viewer set to the account itself.
//!
//! The profile is published whole or not at all: the three retained items are
//! one identity, so a viewer either has a profile or has none.

use crate::storage::{AccountRow, Accounts, Visibility};

/// The identifier this layer's callers name, re-exported.
///
/// So that the `use` list at the top of `web/routes.rs` names
/// `crate::services` and nothing else of this crate.
pub use crate::storage::AccountId;

/// One account, as a viewer is allowed to see it.
///
/// Constructible only through [`of`](Self::of): the fields are private, no
/// derive can build one, and no other function in this crate returns one.
///
/// A viewer who may not see this account gets no profile at all rather than
/// one holding three blanks. The three fields are plain values, so a profile
/// that exists is a whole one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicProfile {
    account_id: AccountId,
    account_name: String,
    avatar_url: String,
}

impl PublicProfile {
    /// `account` as `viewer` may see it, or [`None`] for a viewer who may not.
    ///
    /// The owner — `viewer` equal to this account — sees the profile whatever
    /// the switch says, because the setting is about third parties. A fresh
    /// account is published to nobody.
    ///
    /// `viewer` is [`Option`] because nobody is an ordinary case here: the
    /// spectator pages need no account.
    pub fn of(account: &AccountRow, viewer: Option<AccountId>) -> Option<Self> {
        let owner = viewer == Some(account.account_id);

        (owner || account.visibility.is_published()).then(|| Self {
            account_id: account.account_id,
            account_name: account.account_name.clone(),
            avatar_url: account.avatar_url.clone(),
        })
    }

    /// The GitHub user id.
    pub const fn account_id(&self) -> AccountId {
        self.account_id
    }

    /// The GitHub account name.
    pub fn account_name(&self) -> &str {
        &self.account_name
    }

    /// The URL of the profile image.
    pub fn avatar_url(&self) -> &str {
        &self.avatar_url
    }
}

/// The owner's own account page: their three fields, and the one setting that
/// governs who else sees them.
///
/// The profile is filtered with the owner as viewer, so it is there whatever the
/// setting says; the setting is beside it because it is a **tabia-side setting**
/// rather than GitHub data, and a page that shows a value has to say who else
/// can see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountSettings {
    profile: PublicProfile,
    visibility: Visibility,
}

impl AccountSettings {
    /// The owner's three fields, through the filter every template goes through.
    pub const fn profile(&self) -> &PublicProfile {
        &self.profile
    }

    /// Whether the profile is published to third parties.
    pub const fn publishes_profile(&self) -> bool {
        self.visibility.is_published()
    }
}

/// The account-side half of the web service layer: the store, read through the
/// filter.
///
/// One value rather than a repository the layer above reaches into, and it is
/// what [`Context`](super::Context) holds. Nothing here returns an unfiltered
/// row.
#[derive(Clone, Debug)]
pub struct Profiles {
    accounts: Accounts,
}

impl Profiles {
    /// The account store of an open, migrated database.
    pub const fn new(accounts: Accounts) -> Self {
        Self { accounts }
    }

    /// The owner's own page, or `None` for an id with no account row.
    ///
    /// `None` is what a signed-in identity with no row gets. Sign-in creates
    /// the row, so in a running server the case is "signed in as an account
    /// this database has never seen" — which the page above answers as it
    /// answers anything else that is not there.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn settings(&self, owner: AccountId) -> Result<Option<AccountSettings>, sqlx::Error> {
        Ok(self.accounts.get(owner).await?.map(|row| AccountSettings {
            // The viewer here *is* the owner, and the filter's owner case has no
            // condition on it, so this is the one call site that knows the
            // answer. The `expect` states that fact rather than covering a case.
            profile: PublicProfile::of(&row, Some(owner))
                .expect("the filter shows an account to its own owner"),
            visibility: row.visibility,
        }))
    }

    /// `account` as `viewer` may see it, or `None` for an account this viewer
    /// may not see — including one that is not there.
    ///
    /// **The fetch the participant pages call.** An account with no row and an
    /// account this viewer may not see are one `None`, because they are one
    /// answer on
    /// every page that asks: no identity block at all.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn profile(
        &self,
        account: AccountId,
        viewer: Option<AccountId>,
    ) -> Result<Option<PublicProfile>, sqlx::Error> {
        Ok(self
            .accounts
            .get(account)
            .await?
            .and_then(|row| PublicProfile::of(&row, viewer)))
    }

    /// Publishes the owner's profile, or takes it back, and says whether a row
    /// took it.
    ///
    /// `published` is the state the form asked for, and it becomes a
    /// [`Visibility`] here — the layer above never spells a column and never
    /// writes one of the two words. `false` is "no such account", which is the
    /// only thing left that can be missing.
    ///
    /// **The change is effective at the next read.** Nothing caches an account
    /// row, so the next render of any page is the changed one, with no operator
    /// action and nothing to invalidate.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn set_visibility(
        &self,
        owner: AccountId,
        published: bool,
    ) -> Result<bool, sqlx::Error> {
        self.accounts
            .set_visibility(owner, Visibility::of(published))
            .await
    }

    /// Creates the account or refreshes the two fields GitHub owns — **the
    /// sign-in write**.
    ///
    /// The one caller is the OAuth callback, through
    /// [`Context::sign_in`](super::Context::sign_in), rather than a second
    /// write. A created row publishes nothing, and a refresh leaves the switch
    /// where the owner has put it since.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn sign_in(
        &self,
        account: AccountId,
        account_name: &str,
        avatar_url: &str,
    ) -> Result<(), sqlx::Error> {
        self.accounts
            .sign_in(account, account_name, avatar_url)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::storage::Database;
    use crate::storage::testing::temp_dir;

    const ALICE: AccountId = 4_242;
    const BOB: AccountId = 9_001;

    const NAME: &str = "alice";
    const AVATAR: &str = "https://avatars.example/alice.png";

    /// One account row, with `visibility` as the test wants it.
    ///
    /// Constructed rather than stored: the filter is a pure function of a row
    /// and a viewer, so the tests of it need no database and run under miri.
    fn row(visibility: Visibility) -> AccountRow {
        AccountRow {
            account_id: ALICE,
            account_name: NAME.to_owned(),
            avatar_url: AVATAR.to_owned(),
            visibility,
        }
    }

    #[test]
    fn the_owner_sees_their_own_profile_whatever_the_switch_says() {
        // The setting is about third parties, so it says nothing about what the
        // owner sees.
        for visibility in [Visibility::OwnerOnly, Visibility::Published] {
            let account = row(visibility);

            let profile =
                PublicProfile::of(&account, Some(ALICE)).expect("the owner sees their own");

            assert_eq!(profile.account_id(), ALICE, "{visibility:?}");
            assert_eq!(profile.account_name(), NAME, "{visibility:?}");
            assert_eq!(profile.avatar_url(), AVATAR, "{visibility:?}");
        }
    }

    #[test]
    fn a_fresh_account_shows_a_third_party_nothing_at_all() {
        // A fresh account leaks nothing. Both kinds of third party — another
        // account, and nobody — because the rule is the same for each.
        let account = row(Visibility::OwnerOnly);

        for viewer in [Some(BOB), None] {
            assert_eq!(PublicProfile::of(&account, viewer), None, "{viewer:?}");
        }
    }

    #[test]
    fn a_third_party_sees_the_whole_profile_or_none_of_it() {
        // A participant page shows either the full GitHub box — all three items
        // — or no GitHub box at all. The two states, over both kinds of third
        // party, and there is no third state, because a profile that exists
        // holds all three values.
        for viewer in [Some(BOB), None] {
            assert_eq!(
                PublicProfile::of(&row(Visibility::OwnerOnly), viewer),
                None,
                "{viewer:?}"
            );

            let profile = PublicProfile::of(&row(Visibility::Published), viewer)
                .expect("a published profile is shown");

            assert_eq!(profile.account_id(), ALICE, "{viewer:?}");
            assert_eq!(profile.account_name(), NAME, "{viewer:?}");
            assert_eq!(profile.avatar_url(), AVATAR, "{viewer:?}");
        }
    }

    #[test]
    fn a_published_value_is_the_stored_value() {
        // The filter decides whether the profile is shown, and changes nothing
        // about it when it is.
        let account = row(Visibility::Published);

        let profile = PublicProfile::of(&account, None).expect("published");

        assert_eq!(profile.account_id(), ALICE);
        assert_eq!(profile.account_name(), NAME);
        assert_eq!(profile.avatar_url(), AVATAR);
    }

    /// This module's own source, minus its tests.
    ///
    /// Embedded at compile time, so the scan below reads no file at run time and
    /// runs under miri with the filter's tests beside it.
    fn filter_source() -> &'static str {
        include_str!("privacy.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("a split always yields a first part")
    }

    /// The body of the block `header` opens, up to the closing brace in column
    /// zero.
    fn block(header: &str) -> &'static str {
        let source = filter_source();
        let (_, after) = source
            .split_once(header)
            .unwrap_or_else(|| panic!("{header} is not in this module"));

        after
            .split_once("\n}")
            .unwrap_or_else(|| panic!("{header} is not closed"))
            .0
    }

    #[test]
    fn public_profile_has_no_constructor_but_the_filter() {
        // No template can render an unfiltered account, and no runtime test can
        // observe a constructor that is not there — so this asserts the absence
        // of any other constructor. The claim is about what this module offers,
        // so what is inspected is the module.
        let filter = block("impl PublicProfile {");

        assert_eq!(
            filter.matches("-> Option<Self>").count(),
            1,
            "a second function returns a PublicProfile:\n{filter}"
        );
        assert_eq!(
            filter.matches("-> Self").count(),
            0,
            "a function hands one over unconditionally:\n{filter}"
        );
        // Once, in `of`'s body: the one literal that builds one. A second would
        // be a second place that builds one.
        assert_eq!(
            filter.matches("Self {").count(),
            1,
            "a second place builds one:\n{filter}"
        );
        assert!(
            filter.contains(
                "pub fn of(account: &AccountRow, viewer: Option<AccountId>) -> Option<Self>"
            ),
            "the one constructor is not the filter:\n{filter}"
        );

        // Nor a way around it: a public field would let a caller assemble one,
        // and a derive or an impl could hand one over ready-made.
        let declared = block("pub struct PublicProfile {");
        assert!(!declared.contains("pub "), "{declared}");

        let source = filter_source();
        for other in [
            "impl Default for PublicProfile",
            "impl From<",
            "derive(Default",
        ] {
            assert!(!source.contains(other), "{other} is in this module");
        }
        // And nothing outside that block writes the literal either. The three
        // lines a `PublicProfile {` may appear on are the declaration, the impl
        // header, and the accessor that lends one out by reference.
        for line in source.lines() {
            assert!(
                !line.contains("PublicProfile {")
                    || line.starts_with("pub struct PublicProfile {")
                    || line.starts_with("impl PublicProfile {")
                    || line.contains("-> &PublicProfile {"),
                "a PublicProfile is built outside the filter: {line}"
            );
        }
    }

    /// A fresh database, and a `Profiles` over it.
    async fn fresh(name: &str) -> (PathBuf, Profiles) {
        let dir = temp_dir(&format!("services-privacy-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Database::open(dir.join("tabia.sqlite3"))
            .await
            .expect("a fresh file opens");

        (dir, Profiles::new(Accounts::of(&database)))
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_owners_settings_are_their_three_fields_and_a_switch_at_owner_only() {
        let (dir, profiles) = fresh("settings").await;
        profiles
            .sign_in(ALICE, NAME, AVATAR)
            .await
            .expect("it inserts");

        let settings = profiles
            .settings(ALICE)
            .await
            .expect("the store answers")
            .expect("the row is there");

        assert_eq!(settings.profile().account_id(), ALICE);
        assert_eq!(settings.profile().account_name(), NAME);
        assert_eq!(settings.profile().avatar_url(), AVATAR);
        assert!(!settings.publishes_profile());

        // An identity with no row of its own has no settings rather than an
        // error, and no default row is created by asking.
        assert_eq!(profiles.settings(BOB).await.expect("answers"), None);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_switch_a_form_flipped_is_read_back_at_once() {
        // "Changing a setting takes effect on the public pages with no
        // operator action." Nothing caches a row, so the next read is the
        // changed one.
        let (dir, profiles) = fresh("set").await;
        profiles
            .sign_in(ALICE, NAME, AVATAR)
            .await
            .expect("it inserts");

        assert!(
            profiles
                .set_visibility(ALICE, true)
                .await
                .expect("the store answers")
        );

        assert!(
            profiles
                .settings(ALICE)
                .await
                .expect("answers")
                .expect("the row is there")
                .publishes_profile()
        );

        // And a third party sees the whole profile, at the same moment.
        let seen = profiles
            .profile(ALICE, Some(BOB))
            .await
            .expect("answers")
            .expect("it is published");
        assert_eq!(seen.account_id(), ALICE);
        assert_eq!(seen.account_name(), NAME);
        assert_eq!(seen.avatar_url(), AVATAR);

        // Taking it back is the same call the other way round.
        assert!(
            profiles
                .set_visibility(ALICE, false)
                .await
                .expect("answers")
        );
        assert_eq!(
            profiles.profile(ALICE, Some(BOB)).await.expect("answers"),
            None
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_change_to_an_account_this_server_does_not_have_writes_nothing() {
        // The one thing that can be missing: the wire carries no field name, so
        // "no such account" is the whole of the `false`.
        let (dir, profiles) = fresh("unknown").await;
        profiles
            .sign_in(ALICE, NAME, AVATAR)
            .await
            .expect("it inserts");

        assert!(!profiles.set_visibility(BOB, true).await.expect("answers"));

        // And the account that is there was not touched on the way.
        assert!(
            !profiles
                .settings(ALICE)
                .await
                .expect("answers")
                .expect("the row is there")
                .publishes_profile()
        );
        assert_eq!(profiles.profile(BOB, None).await.expect("answers"), None);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_profile_a_viewer_fetches_is_filtered_before_it_is_returned() {
        // Filtering at the fetch rather than at the render: there is no method
        // here that returns a row, so a caller cannot hold an unfiltered one.
        let (dir, profiles) = fresh("fetch").await;
        profiles
            .sign_in(ALICE, NAME, AVATAR)
            .await
            .expect("it inserts");

        // Owner-only: the owner fetches their own, and nobody else fetches
        // anything.
        let owner = profiles
            .profile(ALICE, Some(ALICE))
            .await
            .expect("answers")
            .expect("the owner sees their own");
        assert_eq!(owner.account_name(), NAME);

        for viewer in [Some(BOB), None] {
            assert_eq!(
                profiles.profile(ALICE, viewer).await.expect("answers"),
                None,
                "{viewer:?}"
            );
        }

        profiles
            .set_visibility(ALICE, true)
            .await
            .expect("the store answers");

        for viewer in [Some(BOB), None] {
            let stranger = profiles
                .profile(ALICE, viewer)
                .await
                .expect("answers")
                .expect("it is published");

            assert_eq!(stranger.account_id(), ALICE, "{viewer:?}");
            assert_eq!(stranger.account_name(), NAME, "{viewer:?}");
            assert_eq!(stranger.avatar_url(), AVATAR, "{viewer:?}");
        }

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
