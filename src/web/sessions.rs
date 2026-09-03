//! The session layer of the web half: the server-side records, and the signed
//! cookie over them.
//!
//! The shape is fixed: a signed, `HttpOnly`, `Secure`, `SameSite=Lax` cookie
//! carries an opaque session ID, and the record it names lives server-side.
//! `SameSite=Lax` still permits the OAuth callback redirect, which `Strict`
//! would break.
//!
//! The records are in memory, so a restart signs everyone out: a sign-in is one
//! redirect that returns immediately, because the visitor's GitHub session is
//! not this server's to lose, and the one thing a `sessions` table would make
//! durable is a credential-equivalent.
//!
//! The lock is never held across an `await`: every method below takes the mutex,
//! does map work, and drops it before returning, and the exchange with
//! github.com happens between two of those calls.
//!
//! Expiry is swept on write, by a linear pass over the map every time a session
//! is created or destroyed. A read never sweeps: a request that finds an expired
//! record treats it as absent and leaves it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, header};
use subtle::ConstantTimeEq;

use crate::auth::{CookieKey, opaque_id};
use crate::services::AccountId;

/// The cookie's name.
///
/// Prefixed with the server's own name so that a browser holding cookies for
/// several things on one host has no collision to arrange around.
pub const COOKIE_NAME: &str = "tabia_session";

/// How long a sign-in attempt may sit between the redirect and the callback.
///
/// Ten minutes: long enough to authorize on github.com, create the account if it
/// is somebody's first visit, and answer a two-factor prompt; short enough that
/// an attempt abandoned in a closed tab is not a record that lives for hours. An
/// attempt that expires is a refusal the visitor answers by clicking sign-in
/// again.
pub const ATTEMPT_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// How long an established session lasts.
///
/// Twelve hours, which is a working day: an engine developer who signs in to
/// issue a token in the morning is still signed in in the evening, and a machine
/// left unattended overnight is not. There is no renewal on activity, so a
/// session's life is measured from the sign-in that created it.
pub const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

/// One server-side session record.
///
/// Both stages are the same record, because the `state` is stored server-side
/// against the pre-login session. Before the callback, `account` is `None` and
/// `state` is the attempt's; after it, the record is a different one — see
/// [`Sessions::take_attempt`] for why the identifier rotates rather than being
/// filled in.
#[derive(Clone, Debug)]
struct Record {
    /// Who this session is, or `None` while it is only an attempt.
    account: Option<AccountId>,

    /// The attempt's `state`, or `None` for an established session.
    state: Option<String>,

    /// When this record stops counting, whatever it holds.
    expires_at: Instant,
}

/// The web half's sessions: the records, and the key their cookies are signed
/// with.
///
/// One per process, held by [`SignIn`](super::sso::SignIn). Not [`Clone`]: a
/// cookie established against a second copy would be anonymous against the
/// first.
#[derive(Debug)]
pub struct Sessions {
    key: CookieKey,
    attempt_lifetime: Duration,
    session_lifetime: Duration,
    live: Mutex<HashMap<String, Record>>,
}

impl Sessions {
    /// The store a running server uses: the configured key and the two constants
    /// above.
    pub fn new(key: CookieKey) -> Self {
        Self::with_lifetimes(key, ATTEMPT_LIFETIME, SESSION_LIFETIME)
    }

    /// The same with stated lifetimes — what a test about expiry needs.
    ///
    /// A parameter rather than an injected clock: a store built with a
    /// zero-length attempt lifetime asserts the expiry rule with no clock to
    /// drive and no `sleep` to wait out.
    pub fn with_lifetimes(
        key: CookieKey,
        attempt_lifetime: Duration,
        session_lifetime: Duration,
    ) -> Self {
        Self {
            key,
            attempt_lifetime,
            session_lifetime,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Starts a sign-in attempt: the cookie to set, and the `state` to send.
    ///
    /// The `state` is generated per attempt from the same OS entropy a session
    /// id is, and is held against the record this creates rather than being
    /// signed into the cookie.
    ///
    /// The record carries no account, so nothing that reads a session for an
    /// identity can be satisfied by one of these.
    pub fn begin_attempt(&self) -> (String, String) {
        let id = opaque_id();
        let state = opaque_id();

        self.insert(
            id.clone(),
            Record {
                account: None,
                state: Some(state.clone()),
                expires_at: Instant::now() + self.attempt_lifetime,
            },
        );

        (self.set_cookie(&id, self.attempt_lifetime), state)
    }

    /// Takes the attempt `cookie` names, if it is a live one.
    ///
    /// Removal is what makes the `state` single-use: the record is gone before
    /// its `state` is compared against anything, so a replayed callback URL
    /// finds nothing. It is also what rotates the session identifier, so an id
    /// fixed by an attacker before the sign-in is not the one that ends up
    /// signed in.
    ///
    /// `None` covers a cookie that is not this server's, an attempt that has
    /// expired, an id that was never issued, and an established session
    /// presented as an attempt.
    ///
    /// Only an attempt is removed: a callback URL is a link anyone can send, and
    /// taking a session here would let a stray one sign a visitor out.
    pub fn take_attempt(&self, cookie: &str) -> Option<String> {
        let id = self.key.verify(cookie)?;
        let mut live = self.live.lock().expect("the session map is not poisoned");

        if !live.get(id).is_some_and(|record| record.state.is_some()) {
            return None;
        }
        let record = live.remove(id)?;

        (record.expires_at > Instant::now())
            .then_some(record.state)
            .flatten()
    }

    /// Whether `presented` is the `state` this attempt was started with.
    ///
    /// Constant-time, through the same `subtle` primitive `auth::token::verify`
    /// uses: a `==` over the bytes short-circuits at the first difference, so
    /// how long it takes says how long a prefix matched.
    pub fn matches(expected: &str, presented: &str) -> bool {
        expected.as_bytes().ct_eq(presented.as_bytes()).into()
    }

    /// Establishes a session for `account`, and returns the cookie to set.
    ///
    /// A fresh identifier, not the attempt's — see
    /// [`take_attempt`](Self::take_attempt) for why.
    pub fn establish(&self, account: AccountId) -> String {
        let id = opaque_id();

        self.insert(
            id.clone(),
            Record {
                account: Some(account),
                state: None,
                expires_at: Instant::now() + self.session_lifetime,
            },
        );

        self.set_cookie(&id, self.session_lifetime)
    }

    /// Who `cookie` is signed in as, or `None`.
    ///
    /// `None` for a cookie this key did not sign, for an id no record has, for a
    /// record that has expired, and for an attempt that never completed.
    ///
    /// The signature is checked first, so a value a client made up is refused
    /// without the map being touched at all.
    pub fn account_of(&self, cookie: &str) -> Option<AccountId> {
        let id = self.key.verify(cookie)?;
        let live = self.live.lock().expect("the session map is not poisoned");
        let record = live.get(id)?;

        (record.expires_at > Instant::now())
            .then_some(record.account)
            .flatten()
    }

    /// Destroys the session `cookie` names, and says whether there was one.
    ///
    /// The server-side record is what a sign-out removes; clearing the cookie is
    /// the layer above's, through [`clear_cookie`](Self::clear_cookie). Doing
    /// only the second would leave a session a copy of the cookie still reaches.
    pub fn destroy(&self, cookie: &str) -> bool {
        let Some(id) = self.key.verify(cookie) else {
            return false;
        };
        let id = id.to_owned();

        let mut live = self.live.lock().expect("the session map is not poisoned");
        let destroyed = live.remove(&id).is_some();
        sweep(&mut live);

        destroyed
    }

    /// The `Set-Cookie` value that establishes `id` for `lifetime`.
    ///
    /// - `HttpOnly` — no script may read it. This server serves no script on any
    ///   page, so the attribute is about what an injected one could do.
    /// - `Secure` — never sent over plaintext HTTP. The web half is itself
    ///   plaintext behind a reverse proxy that terminates TLS, so this is about
    ///   the browser's side of that proxy.
    /// - `SameSite=Lax` — `Strict` would break the OAuth callback redirect. It
    ///   is also what makes `POST /sign-out` safe with no CSRF token, since a
    ///   cross-site `POST` carries no cookie under it.
    /// - `Path=/` — every route of this server, since the middleware runs over
    ///   all of them.
    /// - `Max-Age` — the record's own lifetime, so a browser drops the cookie at
    ///   about the moment the server stops honouring it.
    fn set_cookie(&self, id: &str, lifetime: Duration) -> String {
        format!(
            "{COOKIE_NAME}={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={}",
            self.key.signed(id),
            lifetime.as_secs(),
        )
    }

    /// The `Set-Cookie` value that removes it.
    ///
    /// The same attributes with an empty value and `Max-Age=0`. A browser
    /// matches a cookie for replacement on its name, path and domain, so the
    /// attributes have to agree with the ones it was set under — a `Path`
    /// omitted here would leave the original cookie in place beside a second,
    /// deleted one.
    pub fn clear_cookie() -> String {
        format!("{COOKIE_NAME}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
    }

    /// Inserts a record, sweeping what has expired on the way past.
    fn insert(&self, id: String, record: Record) {
        let mut live = self.live.lock().expect("the session map is not poisoned");
        sweep(&mut live);
        live.insert(id, record);
    }

    /// How many records are held, expired ones included.
    ///
    /// For the tests. Nothing in the server reads it.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.live
            .lock()
            .expect("the session map is not poisoned")
            .len()
    }
}

/// Removes every record whose moment has passed.
///
/// Free of `self` so that it can be called with the lock already held, which is
/// the only way it is ever called.
fn sweep(live: &mut HashMap<String, Record>) {
    let now = Instant::now();
    live.retain(|_, record| record.expires_at > now);
}

/// The value of the session cookie on a request, if it carries one.
///
/// Hand-written rather than taken from the `cookie` crate, whose quoted values,
/// attribute parsing and jar this needs none of. A request may carry more than
/// one `Cookie` header, so all of them are searched.
pub fn presented(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == COOKIE_NAME)
        .map(|(_, value)| value.trim())
}

/// A `Set-Cookie` header value, or `None` if it will not go in a header.
///
/// Everything this module builds is ASCII by construction, so `None` is
/// unreachable in a running server. An `Option` rather than an `expect` because
/// the caller is a request handler.
pub fn header_value(cookie: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(cookie).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::auth::cookie;

    const ALICE: AccountId = 4_242;
    const BOB: AccountId = 9_001;

    /// A key of the right shape, and not one this server would ever generate.
    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn key() -> CookieKey {
        CookieKey::parse(KEY).expect("64 lowercase hex characters")
    }

    fn sessions() -> Sessions {
        Sessions::new(key())
    }

    /// The cookie's value, out of the `Set-Cookie` line that establishes it.
    fn value_of(set_cookie: &str) -> String {
        set_cookie
            .strip_prefix(&format!("{COOKIE_NAME}="))
            .and_then(|rest| rest.split(';').next())
            .unwrap_or_else(|| panic!("not a Set-Cookie for this server: {set_cookie}"))
            .to_owned()
    }

    /// A request carrying `cookie` under this server's name.
    fn carrying(cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{COOKIE_NAME}={cookie}")).expect("the value is ASCII"),
        );

        headers
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn an_established_session_is_the_account_it_was_established_for() {
        let sessions = sessions();

        let cookie = value_of(&sessions.establish(ALICE));

        assert_eq!(sessions.account_of(&cookie), Some(ALICE));
        // And it is one account, not any account: a second session is its own.
        let other = value_of(&sessions.establish(BOB));
        assert_eq!(sessions.account_of(&other), Some(BOB));
        assert_eq!(sessions.account_of(&cookie), Some(ALICE));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn an_attempt_is_not_a_session() {
        // The record exists and carries no account.
        let sessions = sessions();

        let (set_cookie, _state) = sessions.begin_attempt();

        assert_eq!(sessions.account_of(&value_of(&set_cookie)), None);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn an_attempts_state_is_taken_once() {
        // Single-use by the record's removal: the second take finds nothing.
        let sessions = sessions();
        let (set_cookie, state) = sessions.begin_attempt();
        let cookie = value_of(&set_cookie);

        assert_eq!(sessions.take_attempt(&cookie), Some(state.clone()));
        assert_eq!(sessions.take_attempt(&cookie), None);
        // And the cookie is no longer anything at all afterwards.
        assert_eq!(sessions.account_of(&cookie), None);
        assert_ne!(state, String::new());
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn an_expired_attempt_is_not_a_live_one() {
        // A zero lifetime, so there is no clock to drive and no sleep to wait
        // out.
        let sessions = Sessions::with_lifetimes(key(), Duration::ZERO, SESSION_LIFETIME);
        let (set_cookie, _state) = sessions.begin_attempt();

        assert_eq!(sessions.take_attempt(&value_of(&set_cookie)), None);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn an_expired_session_is_anonymous() {
        let sessions = Sessions::with_lifetimes(key(), ATTEMPT_LIFETIME, Duration::ZERO);

        let cookie = value_of(&sessions.establish(ALICE));

        assert_eq!(sessions.account_of(&cookie), None);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn an_established_session_cannot_be_taken_as_an_attempt_and_survives_the_asking() {
        // A callback URL is a link anyone can send, so a stray one presented
        // with a live session must not sign that session out.
        let sessions = sessions();

        let cookie = value_of(&sessions.establish(ALICE));

        assert_eq!(sessions.take_attempt(&cookie), None);
        assert_eq!(sessions.account_of(&cookie), Some(ALICE));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_tampered_cookie_is_anonymous_rather_than_an_error() {
        let sessions = sessions();
        let cookie = value_of(&sessions.establish(ALICE));
        let (id, mac) = cookie.rsplit_once('.').expect("it was just signed");

        // An id somebody made up, signed with nothing; the right id with a MAC
        // that is not this key's; a cookie that is not one at all.
        for presented in [
            format!("{}.{mac}", cookie::opaque_id()),
            format!("{id}.{}", "0".repeat(64)),
            id.to_owned(),
            String::new(),
        ] {
            assert_eq!(sessions.account_of(&presented), None, "{presented}");
            assert_eq!(sessions.take_attempt(&presented), None, "{presented}");
            assert!(!sessions.destroy(&presented), "{presented}");
        }

        // The untouched cookie still is what it was, so the refusals above are
        // about the edits.
        assert_eq!(sessions.account_of(&cookie), Some(ALICE));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn destroying_a_session_makes_its_cookie_anonymous() {
        let sessions = sessions();
        let cookie = value_of(&sessions.establish(ALICE));

        assert!(sessions.destroy(&cookie));

        assert_eq!(sessions.account_of(&cookie), None);
        assert_eq!(sessions.held(), 0);
        // A second sign-out has nothing to destroy and says so.
        assert!(!sessions.destroy(&cookie));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn expired_records_are_swept_when_something_is_written() {
        // A read never sweeps, so what bounds the store is the next sign-in.
        let sessions = Sessions::with_lifetimes(key(), Duration::ZERO, Duration::ZERO);
        for _ in 0..8 {
            sessions.establish(ALICE);
        }
        assert_eq!(sessions.held(), 1, "each insert swept the one before it");

        let live = Sessions::new(key());
        for _ in 0..8 {
            live.establish(ALICE);
        }
        assert_eq!(live.held(), 8, "nothing live is swept");
    }

    #[test]
    fn the_state_comparison_is_total_over_lengths() {
        assert!(Sessions::matches("a-state", "a-state"));
        assert!(!Sessions::matches("a-state", "a-statf"));
        assert!(!Sessions::matches("a-state", "a-stat"));
        assert!(!Sessions::matches("a-state", "a-statee"));
        assert!(!Sessions::matches("a-state", ""));
        assert!(Sessions::matches("", ""));
    }

    #[test]
    fn the_cookie_carries_every_attribute_a_session_cookie_needs() {
        let sessions = sessions();

        let set_cookie = sessions.establish(ALICE);

        for attribute in ["Path=/", "HttpOnly", "Secure", "SameSite=Lax"] {
            assert!(set_cookie.contains(attribute), "{attribute}: {set_cookie}");
        }
        assert!(
            set_cookie.contains(&format!("Max-Age={}", SESSION_LIFETIME.as_secs())),
            "{set_cookie}"
        );

        // And the one that removes it agrees on all of them, since a browser
        // matches a replacement on name and path.
        let cleared = Sessions::clear_cookie();
        for attribute in ["Path=/", "HttpOnly", "Secure", "SameSite=Lax", "Max-Age=0"] {
            assert!(cleared.contains(attribute), "{attribute}: {cleared}");
        }
        assert!(
            cleared.starts_with(&format!("{COOKIE_NAME}=;")),
            "{cleared}"
        );
    }

    #[test]
    fn the_cookie_is_read_out_of_whatever_the_request_carries() {
        assert_eq!(presented(&carrying("a-value")), Some("a-value"));

        // Beside others, in either order, and with the spacing a browser sends.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; tabia_session=a-value; another=2"),
        );
        assert_eq!(presented(&headers), Some("a-value"));

        // A name that merely contains this one is not this one.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("not_tabia_session=a-value"),
        );
        assert_eq!(presented(&headers), None);

        assert_eq!(presented(&HeaderMap::new()), None);
    }
}
