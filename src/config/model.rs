//! The server's own TOML configuration, as deserialized.
//!
//! A position collection is data rather than configuration and lives in the
//! plain-text file this configuration points at; nothing here reads that file.
//!
//! Numbers are counts of `time_unit`. An operator writes the numbers that will
//! appear on the wire, and the multiplication into a [`Duration`] happens
//! once, here, so the configured numbers and the `Time` block's numbers
//! coincide by construction.
//!
//! Unknown anything is an error: an unknown key at any level, an unknown
//! `auth_mode`, `time_unit` or `side` each fails parsing naming the offender.
//! A typo that was silently ignored would be a setting that never takes
//! effect, which an operator cannot see.

use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::game::Color;

use super::timestamp::FirstRound;

/// The whole of what the configuration file says.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Which authentication mode the instance runs.
    ///
    /// Defaults to [`AuthMode::Open`], the mode an instance can serve with
    /// nothing else configured: `github` needs a GitHub OAuth app, a client id
    /// and two environment variables.
    #[serde(default)]
    pub auth_mode: AuthMode,

    /// The position collection to play from.
    ///
    /// Whether the file is there, is readable, and holds usable entries is
    /// [`Collection::load`]'s answer, at the point where it is opened.
    ///
    /// [`Collection::load`]: crate::storage::Collection::load
    pub positions: PathBuf,

    /// The directory every finished game's record file is written into.
    ///
    /// Relative paths resolve against the process's working directory. Created
    /// if it is not there, and [`Records::open`] answers whether this process
    /// can write in it by writing.
    ///
    /// [`Records::open`]: crate::storage::Records::open
    pub records: PathBuf,

    /// The SQLite file every finished game gets a row in.
    ///
    /// Relative paths resolve against the process's working directory. Created
    /// if it is not there, and migrated at every startup, so an operator who
    /// names a new path gets an empty history rather than a failure.
    pub database: PathBuf,

    /// The `Max_Moves` limit and the playable remainder it implies. Absent
    /// means no limit, and with no limit the margin rule has nothing to say.
    pub limit: Option<Limit>,

    /// The time settings every game is played under. The whole table defaults,
    /// because every key in it does.
    #[serde(default)]
    pub time: TimeConfig,

    /// How the CSA listener runs: where it listens, and the two limits the
    /// runtime enforces per connection and per pairing. The whole table
    /// defaults.
    #[serde(default)]
    pub csa: CsaConfig,

    /// When matchmaking rounds run. The whole table defaults.
    #[serde(default)]
    pub matchmaking: MatchmakingConfig,

    /// What an account may hold: the two per-account token caps. The whole
    /// table defaults.
    #[serde(default)]
    pub accounts: AccountsConfig,

    /// How often the two rating tables are updated. The whole table defaults,
    /// and it holds one key: the algorithm's own numbers are not
    /// configuration.
    #[serde(default)]
    pub ratings: RatingsConfig,

    /// Where the web half listens, and who administers it. The HTTP listener
    /// always runs, so this table says how rather than whether, and the whole
    /// of it defaults.
    #[serde(default)]
    pub web: WebConfig,
}

impl Config {
    /// Parses a configuration's TOML text.
    ///
    /// Text rather than a path, so that the parser is testable with no
    /// filesystem. The error is `toml`'s own, unwrapped: it already names the
    /// offending key and where in the file it sits.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// The `agreement_timeout_seconds` default: 120 seconds.
///
/// The same number as [`AGREEMENT_TIMEOUT`], which `config` cannot name
/// because the edge would point at `session`.
///
/// [`AGREEMENT_TIMEOUT`]: crate::session::AGREEMENT_TIMEOUT
const fn default_agreement_timeout_seconds() -> u64 {
    120
}

/// The CSA listener's default host: every interface, since engines connect
/// from other machines.
fn default_csa_host() -> String {
    "0.0.0.0".to_owned()
}

/// The CSA listener's default port: 4081, the port floodgate listens on, so a
/// client already configured for floodgate needs one fewer change.
const fn default_csa_port() -> u16 {
    4_081
}

/// The `max_malformed_lines` default: eight lines before a connection closes.
///
/// Enough that a client with a bug in one message is not disconnected for it,
/// and few enough that a client sending nothing this server understands stops
/// occupying a connection.
fn default_max_malformed_lines() -> NonZeroU32 {
    NonZeroU32::new(8).expect("8 is not zero")
}

/// How the CSA listener runs: where it listens, and the two limits the runtime
/// enforces.
///
/// These are operational settings, not game time, so `time_unit` does not
/// apply to any of them and the one duration key carries its unit in its name.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CsaConfig {
    /// The interface the CSA listener binds.
    ///
    /// An IPv6 literal is written bare — `host = "::1"`, not `"[::1]"`;
    /// [`listen`](Self::listen) puts the brackets back.
    ///
    /// Whether the value resolves and whether the address is bindable are
    /// answered where it is bound.
    #[serde(default = "default_csa_host")]
    pub host: String,

    /// The port the CSA listener binds.
    ///
    /// `0` binds an ephemeral port: the bound address is read back from the
    /// handle [`run`](crate::run) returns.
    #[serde(default = "default_csa_port")]
    pub port: u16,

    /// How many malformed lines close a connection.
    ///
    /// [`NonZeroU32`] rather than a `u32` with a check: a limit of zero would
    /// close every connection before its first line, and serde refuses it by
    /// name where it is written.
    #[serde(default = "default_max_malformed_lines")]
    pub max_malformed_lines: NonZeroU32,

    /// How long an offered pairing waits for both agreements, in seconds.
    ///
    /// The default is 120 seconds, matching shogi-server.
    #[serde(default = "default_agreement_timeout_seconds")]
    pub agreement_timeout_seconds: u64,

    /// The certificate and key the CSA listener serves TLS with.
    ///
    /// Its presence is the switch: written, the listener is TLS; absent, the
    /// listener is plaintext TCP. Whether the files exist and form a usable
    /// pair is [`Transport::new`]'s answer, at the point where they are read.
    ///
    /// [`Transport::new`]: crate::session::Transport::new
    pub tls: Option<TlsConfig>,
}

impl Default for CsaConfig {
    fn default() -> Self {
        Self {
            host: default_csa_host(),
            port: default_csa_port(),
            max_malformed_lines: default_max_malformed_lines(),
            agreement_timeout_seconds: default_agreement_timeout_seconds(),
            tls: None,
        }
    }
}

/// A host and a port, as an address something can be bound to.
///
/// An operator who writes `host = "::1"` has nowhere to put the brackets, and
/// `"::1:8080"` resolves to nothing, so a host that holds a `:` is bracketed
/// here and an IPv6 literal is written bare in the file.
fn listen_address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Where the CSA listener's TLS material lives.
///
/// Both keys are required, because neither is usable alone: a certificate with
/// no key cannot complete a handshake, and a key with no certificate has
/// nothing to present.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// The PEM certificate chain to present, leaf first.
    pub cert: PathBuf,

    /// The PEM private key belonging to that chain's leaf.
    pub key: PathBuf,
}

/// The web half's default host: loopback.
///
/// **Not [`default_csa_host`]'s answer, and deliberately.** This half is
/// plaintext HTTP, with TLS terminated by a reverse proxy, so an operator who
/// has said nothing gets a listener the proxy on the same host can reach and the
/// network cannot. Serving unencrypted HTTP to the world is a decision, and a
/// decision is something an operator writes down.
fn default_web_host() -> String {
    "127.0.0.1".to_owned()
}

/// The web half's default port: 8080, the conventional plaintext HTTP port for
/// something behind a proxy.
const fn default_web_port() -> u16 {
    8_080
}

/// Where the web half listens.
///
/// There is no way to turn the HTTP listener off: every key here defaults, so
/// an omitted `[web]` and a `[web]` written with nothing in it are the same
/// instance, an HTTP listener on `127.0.0.1:8080`.
///
/// Plaintext HTTP, with no `[web.tls]` to mirror `[csa.tls]`: TLS for the web
/// half is a deployment matter that a reverse proxy terminates, where a CSA
/// client speaks to this process directly.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// The interface the HTTP listener binds.
    ///
    /// [`CsaConfig::host`]'s rules — an IPv6 literal bare, the brackets put
    /// back by [`listen`](Self::listen) — with a different default.
    #[serde(default = "default_web_host")]
    pub host: String,

    /// The port the HTTP listener binds. `0` binds an ephemeral port.
    #[serde(default = "default_web_port")]
    pub port: u16,

    /// The GitHub OAuth app this instance signs visitors in through, or
    /// `None`.
    ///
    /// Required in `github` mode and pointless in `open` mode: a `github`-mode
    /// instance with no OAuth app verifies logins against tokens nothing can
    /// issue, so its absence is a startup failure ([`StartupError::Sso`]),
    /// while an `open`-mode instance has no account for a sign-in to be.
    ///
    /// [`StartupError::Sso`]: crate::StartupError::Sso
    pub oauth: Option<OauthConfig>,

    /// The GitHub accounts that administer this instance, by user id.
    ///
    /// An administrator is two things on this server: a signed-in session that
    /// reaches the admin page, where the designated ratings of engines that are
    /// not presets are set, and an account that issues tokens with
    /// `[accounts]`'s two caps not consulted.
    ///
    /// A user id and not a login name, because a login name freed by a rename
    /// can be claimed by somebody else, which would hand this list's authority
    /// to a stranger without a line of the file changing. An operator reads
    /// their own id once, from `https://api.github.com/users/<login>`.
    ///
    /// `github` mode only: a list written on an `open`-mode instance is a
    /// startup warning ([`Warning::AdministratorsWithoutGithubMode`]) rather
    /// than a failure.
    ///
    /// [`Warning::AdministratorsWithoutGithubMode`]:
    ///     super::validate::Warning::AdministratorsWithoutGithubMode
    #[serde(default)]
    pub administrators: Vec<i64>,
}

/// The GitHub OAuth app: the half of it that is not a secret.
///
/// One key, because only the client id belongs in a file: it is public, where
/// the client secret and the cookie signing key are read from the environment.
///
/// No `redirect_uri` key: GitHub uses the callback URL registered on the OAuth
/// app when the authorize URL names none, so this server names none rather
/// than keeping a second copy equal to the app registration.
///
/// No endpoint keys either. An operator who could point the token endpoint at
/// another host would be an operator who could send this server's client
/// secret to it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OauthConfig {
    /// The OAuth app's client id, as GitHub issued it.
    ///
    /// Required whenever the table is written. Whether the value is an app
    /// that exists is github.com's answer to give, at the first sign-in.
    pub client_id: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            host: default_web_host(),
            port: default_web_port(),
            oauth: None,
            administrators: Vec::new(),
        }
    }
}

impl WebConfig {
    /// [`host`](Self::host) and [`port`](Self::port) as an address to bind.
    pub fn listen(&self) -> String {
        listen_address(&self.host, self.port)
    }
}

impl CsaConfig {
    /// [`host`](Self::host) and [`port`](Self::port) as an address to bind.
    pub fn listen(&self) -> String {
        listen_address(&self.host, self.port)
    }

    /// [`agreement_timeout_seconds`](Self::agreement_timeout_seconds) as a
    /// duration.
    pub const fn agreement_timeout(&self) -> Duration {
        Duration::from_secs(self.agreement_timeout_seconds)
    }
}

/// The `idle_delay_seconds` default: one minute from a quiet moment to the
/// round that answers it.
const fn default_idle_delay_seconds() -> u64 {
    60
}

/// The `interval_seconds` default: half an hour, the longest a full pool waits.
fn default_interval_seconds() -> NonZeroU64 {
    NonZeroU64::new(1_800).expect("1800 is not zero")
}

/// When matchmaking rounds run: the matchmaking schedule.
///
/// A round is a time, not an event: nothing a client does starts one. The two
/// rules these three keys express are
///
/// 1. the first round after startup runs at
///    [`first_round_at`](Self::first_round_at), or — unset, or already past —
///    at `startup + idle_delay_seconds`;
/// 2. after every round, the next round is at `this round + interval_seconds`,
///    and whenever the server goes from at least one game in progress to none,
///    the next round is brought forward to `that moment + idle_delay_seconds`
///    if that is earlier. The transition is what counts, not what the most
///    recent round did.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchmakingConfig {
    /// How long after a quiet moment — startup, or the last game on the server
    /// ending — the next round runs.
    ///
    /// Zero is a setting, not a mistake: it makes a round follow the pool
    /// immediately.
    #[serde(default = "default_idle_delay_seconds")]
    pub idle_delay_seconds: u64,

    /// The longest gap between two consecutive rounds.
    ///
    /// [`NonZeroU64`] rather than a `u64` with a check: an interval of zero is
    /// a round that is always due.
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: NonZeroU64,

    /// The wall-clock time of the first round after startup.
    ///
    /// Absent, or written as the empty string, means not set. From the second
    /// round on the interval and the idle delay fix the times and this value
    /// is never read again — including across a restart, where a value now in
    /// the past falls back to `startup + idle_delay_seconds`.
    #[serde(default, deserialize_with = "first_round_at")]
    pub first_round_at: Option<FirstRound>,

    /// What an engine with no rating is scored at when a round pairs it.
    ///
    /// The matchmaking estimate scores an unrated engine from its last
    /// opponent where it can, and from this value where it cannot.
    ///
    /// It never leaves the pairing: it enters no fit, reaches no published
    /// table, and is read by no page, so an engine scored at this value is an
    /// unrated engine to every other rule in this server. The one other reader
    /// is the preset supervisor's start-time guess.
    #[serde(default = "default_unrated_estimate")]
    pub unrated_estimate: i32,

    /// The preset engines this instance runs. Absent, or written as `[]`,
    /// means the instance has no preset engines.
    #[serde(default)]
    pub preset_engine_tokens: PresetEngines,
}

/// The `unrated_estimate` default: 3000.
///
/// The same number as [`DEFAULT_RATE`], which `config` cannot name because the
/// edge would point at `session`; a test in this module pins the two together.
///
/// [`DEFAULT_RATE`]: crate::session::matchmaker::DEFAULT_RATE
const fn default_unrated_estimate() -> i32 {
    3_000
}

impl Default for MatchmakingConfig {
    fn default() -> Self {
        Self {
            idle_delay_seconds: default_idle_delay_seconds(),
            interval_seconds: default_interval_seconds(),
            first_round_at: None,
            unrated_estimate: default_unrated_estimate(),
            preset_engine_tokens: PresetEngines::default(),
        }
    }
}

impl MatchmakingConfig {
    /// [`idle_delay_seconds`](Self::idle_delay_seconds) as a duration.
    pub const fn idle_delay(&self) -> Duration {
        Duration::from_secs(self.idle_delay_seconds)
    }

    /// [`interval_seconds`](Self::interval_seconds) as a duration.
    pub const fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_seconds.get())
    }
}

/// The longest token this list accepts, in characters.
///
/// The same bound as `csa::command::MAX_TOKEN_LEN`, where the wire enforces
/// it; `config` cannot name a `csa` type, so a test in this module pins the
/// two together.
const MAX_PRESET_ENGINE_TOKEN_LEN: usize = 64;

/// Which protocol a registered preset speaks, and — through that — who runs it.
///
/// A `csa` entry registers an engine that speaks this server's own protocol:
/// the operator runs it wherever they like and it logs in over the ordinary
/// listener. A `usi` entry registers a plain USI engine, which cannot log in
/// at all, so the server runs it and bridges it.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// `protocol = "csa"`, the default and the meaning of an entry that says
    /// nothing: an engine the **operator** runs, speaking CSA, logging in with
    /// this entry's token.
    #[default]
    Csa,

    /// `protocol = "usi"`: a plain USI engine the **server** runs, played by an
    /// in-process bridge that speaks USI toward the engine and CSA toward this
    /// server's own listener.
    Usi,
}

impl Protocol {
    /// The spelling an operator writes, for a message that has to name the value
    /// an entry carries.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Csa => "csa",
            Self::Usi => "usi",
        }
    }
}

/// When a USI preset's process runs.
///
/// Required for a USI entry and refused for a CSA one, because it is a
/// statement about a process and a CSA entry has none.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Lifecycle {
    /// `lifecycle = "resident"`: started when the server starts, logged in,
    /// and waiting between games. It occupies one of the cap's slots only
    /// while it is in a game — charging it for waiting would mean it could
    /// never play.
    Resident,

    /// `lifecycle = "on-demand"`: started when a round wants it and stopped
    /// after its game, so that nothing of the server's own sits waiting between
    /// rounds. It occupies a slot from the moment its process starts until that
    /// process has been stopped.
    OnDemand,
}

impl Lifecycle {
    /// The spelling an operator writes.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::OnDemand => "on-demand",
        }
    }
}

/// One preset engine, as the operator registers it.
///
/// What the entry may carry follows from the protocol, and the keys that do
/// not apply are refused rather than ignored: a `command` written on a CSA
/// entry would be a command nobody ever runs, and an operator cannot see that
/// from the outside.
///
/// | key | `protocol = "csa"` | `protocol = "usi"` |
/// | --- | --- | --- |
/// | `token` | required | required |
/// | `rating` | optional | optional |
/// | `command` | refused | required |
/// | `lifecycle` | refused | required |
/// | `usi_options` | refused | optional |
/// | `name` | refused | optional |
///
/// A CSA preset is externally run: the server never starts, stops or restarts
/// one, and its part is recognition alone.
///
/// The server passes nothing to the command — no arguments appended and no
/// environment set. In particular the token above is never put on the child's
/// command line, where every process on the host could read it out of `/proc`:
/// the bridge holds the token and presents it over the loopback connection
/// itself.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PresetEngine {
    /// The token this preset presents at `LOGIN`, and the only thing that
    /// makes a session this preset's. Compared byte for byte.
    ///
    /// A USI preset's bridge presents it exactly the way an operator's own
    /// client does, which is what makes a bridged engine's games ordinary
    /// games.
    pub token: String,

    /// Which protocol this preset speaks. `csa` when the key is absent, so an
    /// entry that names no protocol registers an engine the operator runs.
    #[serde(default)]
    pub protocol: Protocol,

    /// The command that runs this preset's USI engine: the program, then its
    /// arguments.
    ///
    /// A list rather than a shell string, so nothing here is word-split or
    /// glob-expanded: the first element is executed and the rest are its
    /// `argv`. A word of the list that is empty is refused at startup.
    ///
    /// Required for a USI entry and refused for a CSA one.
    #[serde(default)]
    pub command: Vec<String>,

    /// When this preset's process runs — required for a USI entry, refused for a
    /// CSA one.
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,

    /// USI options to set, sent as one `setoption` line each between `usi` and
    /// `isready`.
    ///
    /// The order the lines are sent in is the option names' own order rather
    /// than the file's, so that two runs over one file drive the engine
    /// identically. Refused for a CSA entry.
    #[serde(default)]
    pub usi_options: UsiOptions,

    /// The name this preset logs in under, or `None` for the engine's own
    /// `id name`.
    ///
    /// The name a participant page, a rating table and a game record show, so
    /// an operator running two builds of one engine writes it here to tell
    /// them apart. Refused for a CSA entry, which presents its own name in its
    /// own `LOGIN` line.
    #[serde(default)]
    pub name: Option<String>,

    /// The rating the operator designates for this preset, or `None`.
    ///
    /// A designated rating is reference data for choosing the origin of a
    /// published table's scale. It pins nothing: this preset's displayed
    /// rating is the value the fit computes for it, the designated value never
    /// enters the fit, and it constrains no pairing.
    ///
    /// The one other thing it is read for is the start-time guess that chooses
    /// which preset to start for an odd round.
    ///
    /// Configuration decides it and nothing else: a preset has a designated
    /// rating exactly when this key is written. An engine that is not a preset
    /// is designated from the admin page instead, by participant ID and into
    /// the database.
    #[serde(default)]
    pub rating: Option<i32>,
}

impl PresetEngine {
    /// Whether the operator designated a rating for this preset — the presence
    /// of the value, never its magnitude and never any runtime rating.
    pub const fn has_designated_rating(&self) -> bool {
        self.rating.is_some()
    }

    /// Whether this preset is one the server does not run: a CSA entry.
    pub const fn is_externally_run(&self) -> bool {
        matches!(self.protocol, Protocol::Csa)
    }

    /// When this preset's process runs. `None` exactly for a CSA entry, where
    /// the key is refused, so an absence means no process of ours rather than
    /// a default someone forgot to write.
    pub const fn lifecycle(&self) -> Option<Lifecycle> {
        self.lifecycle
    }
}

impl fmt::Debug for PresetEngine {
    /// Everything but the secrets: the token is redacted, and so are the
    /// command's arguments, which may name a file that is nobody else's
    /// business.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PresetEngine")
            .field("token", &"<redacted>")
            .field("protocol", &self.protocol)
            .field(
                "command",
                &format_args!(
                    "{}",
                    match self.command.split_first() {
                        None => "<externally run, no command>".to_owned(),
                        Some((program, arguments)) =>
                            format!("[{program}, {} more arguments, redacted]", arguments.len()),
                    }
                ),
            )
            .field("lifecycle", &self.lifecycle)
            .field(
                "usi_options",
                &format_args!("<{} set>", self.usi_options.len()),
            )
            .field("name", &self.name)
            .field("rating", &self.rating)
            .finish()
    }
}

/// The USI options one entry sets, by option name.
///
/// A [`BTreeMap`](std::collections::BTreeMap) rather than a hash map, so that
/// the `setoption` lines go out in a fixed order.
///
/// The value is a TOML scalar rather than a string, because that is how the
/// options are written — `USI_Hash = 256`, `BookFile = "/opt/book.bin"`. USI
/// itself has one value syntax, the rest of the line, and
/// [`UsiOptions::iter`] is where each scalar is spelled for it.
#[derive(Clone, Default, Debug, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct UsiOptions(std::collections::BTreeMap<String, UsiOptionValue>);

impl UsiOptions {
    /// How many options are set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no option is set at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Each option as `(name, value)`, in the option names' order, with the value
    /// already spelled the way a `setoption` line carries it.
    pub fn iter(&self) -> impl Iterator<Item = (&str, String)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_string()))
    }
}

/// One USI option's value, as TOML writes it.
///
/// Three scalars and no table or array: USI has no syntax for either.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum UsiOptionValue {
    /// A `check` option's `true` or `false`, which USI spells in lowercase.
    Boolean(bool),

    /// A `spin` option's number.
    Integer(i64),

    /// A `string`, `filename` or `combo` option's text, written as it stands.
    Text(String),
}

impl fmt::Display for UsiOptionValue {
    /// The value as a `setoption` line carries it, with no quoting: USI has
    /// none, and the rest of the line is the value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boolean(value) => write!(f, "{value}"),
            Self::Integer(value) => write!(f, "{value}"),
            Self::Text(value) => f.write_str(value),
        }
    }
}

/// The preset engines this instance runs.
///
/// A session is a preset engine exactly when the token it presented is one of
/// these entries' tokens, compared byte for byte.
///
/// A newtype rather than a bare `Vec<PresetEngine>` for two reasons:
///
/// - It holds token material, so it prints none. [`Debug`](fmt::Debug) is
///   hand-written to say how many presets there are and nothing about any of
///   their tokens, because [`Config`] derives `Debug`.
/// - Every entry is checked in [`Deserialize`], where it is read: a token no
///   client could ever present designates nobody, and it would do so silently.
///
/// An error names the position of the offending entry, counted from one,
/// rather than the token itself.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PresetEngines(Vec<PresetEngine>);

impl PresetEngines {
    /// Which preset a presented token designates, as an index into this list.
    ///
    /// Byte for byte and case-sensitive. An empty list answers `None` to
    /// everything, which is the "no preset engines" the absent key means.
    ///
    /// The **index**, not a `bool`: a session that is a preset engine is a
    /// session of one particular preset, and the process supervisor, the fixed
    /// rate and the command all hang off which one.
    pub fn designates(&self, presented: &str) -> Option<usize> {
        self.0.iter().position(|preset| preset.token == presented)
    }

    /// How many presets are registered — for a startup line that says so
    /// without saying which.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the instance registers no preset engines at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One preset by index, or `None` for an index this list has no entry at.
    pub fn get(&self, index: usize) -> Option<&PresetEngine> {
        self.0.get(index)
    }

    /// Every registered preset, in the order the file writes them.
    pub fn iter(&self) -> std::slice::Iter<'_, PresetEngine> {
        self.0.iter()
    }

    /// The rating designated for the preset at `index`, or `None` — for a
    /// preset with none, and for an index this list has no entry at.
    pub fn designated_rating(&self, index: usize) -> Option<i32> {
        self.get(index).and_then(|preset| preset.rating)
    }

    /// Whether the preset at `index` is one the server does not run. An index
    /// this list has no entry at answers `true`, which is the answer that never
    /// asks a process to be started.
    pub fn is_externally_run(&self, index: usize) -> bool {
        self.get(index).is_none_or(PresetEngine::is_externally_run)
    }

    /// When the preset at `index` runs, or `None` — for a CSA entry, which has
    /// no process of this server's, and for an index this list has no entry at,
    /// which has nothing at all.
    pub fn lifecycle(&self, index: usize) -> Option<Lifecycle> {
        self.get(index).and_then(PresetEngine::lifecycle)
    }
}

impl fmt::Debug for PresetEngines {
    /// The count, never a token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} preset engines, tokens redacted>", self.0.len())
    }
}

impl<'de> Deserialize<'de> for PresetEngines {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        let written = Vec::<PresetEngine>::deserialize(deserializer)?;

        for (position, preset) in written.iter().enumerate() {
            let refuse = |fault: &str| {
                Err(D::Error::custom(format!(
                    "[matchmaking].preset_engine_tokens: entry {} {fault}",
                    position + 1
                )))
            };

            if let Some(fault) = unusable(&preset.token) {
                return refuse(fault);
            }

            if preset.command.iter().any(String::is_empty) {
                return refuse("has an empty word in its command");
            }

            if let Some(fault) = misplaced(preset) {
                return refuse(&fault);
            }

            if let Some(earlier) = written[..position]
                .iter()
                .position(|other| other.token == preset.token)
            {
                return Err(D::Error::custom(format!(
                    "[matchmaking].preset_engine_tokens: entry {} repeats the token of entry {}",
                    position + 1,
                    earlier + 1
                )));
            }
        }

        Ok(Self(written))
    }
}

/// Why a designated token could never reach the matchmaker, or `None`.
///
/// The list designates tokens a client presents, so a string no `LOGIN` line
/// can carry designates nothing. The `'` rule is not the charset's: a client
/// line is cut at its first `'`, so `tok'en` arrives as `tok`.
fn unusable(token: &str) -> Option<&'static str> {
    if token.is_empty() {
        return Some("is empty");
    }

    if !token.chars().all(|c| matches!(c, '!'..='~')) {
        return Some(
            "holds a character outside printable ASCII (0x21..=0x7E), which no LOGIN line can carry",
        );
    }

    if token.contains('\'') {
        return Some(
            "holds a ', and everything from the first ' in a client line is a comment, so a client \
             cannot present it whole",
        );
    }

    if token.len() > MAX_PRESET_ENGINE_TOKEN_LEN {
        return Some("is longer than the 64 characters a token may have");
    }

    None
}

/// Which key this entry writes that its protocol has no use for, or leaves out
/// that its protocol requires — or `None`.
///
/// The table on [`PresetEngine`], enforced. The message names the key, so that
/// the fix is the words the operator searches their file for.
fn misplaced(preset: &PresetEngine) -> Option<String> {
    let stated = preset.protocol.as_str();

    match preset.protocol {
        Protocol::Csa => {
            let unusable = [
                (!preset.command.is_empty(), "command"),
                (preset.lifecycle.is_some(), "lifecycle"),
                (!preset.usi_options.is_empty(), "usi_options"),
                (preset.name.is_some(), "name"),
            ];

            unusable.into_iter().find_map(|(written, key)| {
                written.then(|| {
                    format!(
                        "writes `{key}`, which a `protocol = \"{stated}\"` entry has no use for: \
                         the operator runs a CSA engine themselves and this server only recognises \
                         its token"
                    )
                })
            })
        }

        Protocol::Usi => {
            let missing = [
                (preset.command.is_empty(), "command"),
                (preset.lifecycle.is_none(), "lifecycle"),
            ];

            missing.into_iter().find_map(|(absent, key)| {
                absent.then(|| {
                    format!(
                        "does not write `{key}`, which a `protocol = \"{stated}\"` entry requires: \
                         the server runs this engine itself"
                    )
                })
            })
        }
    }
}

/// Reads `first_round_at`, where the empty string is the absent value.
///
/// A hand-written step rather than a `String` field parsed later, so that an
/// invalid configuration fails at startup.
fn first_round_at<'de, D>(deserializer: D) -> Result<Option<FirstRound>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let written = Option::<String>::deserialize(deserializer)?;
    let Some(written) = written.filter(|written| !written.is_empty()) else {
        return Ok(None);
    };

    FirstRound::new(&written)
        .map(Some)
        .map_err(|error| D::Error::custom(format!("{written:?}: {error}")))
}

/// How a CSA login is authenticated.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Only issued, unrevoked tokens are accepted. The mode for a server open
    /// to whoever arrives.
    Github,

    /// Any token string is accepted and no GitHub sign-in is required to play.
    ///
    /// The default, because it is the mode an instance can serve with nothing
    /// else configured.
    #[default]
    Open,
}

/// The `active_token_cap` default: three tokens active at once.
fn default_active_token_cap() -> NonZeroU32 {
    NonZeroU32::new(3).expect("3 is not zero")
}

/// The `lifetime_token_cap` default: sixteen tokens ever issued.
fn default_lifetime_token_cap() -> NonZeroU32 {
    NonZeroU32::new(16).expect("16 is not zero")
}

/// What one account may hold: the two token caps.
///
/// The caps are checked only on the issue path, so a change here is a
/// configuration edit and a restart, never a migration: an account already
/// above a lowered cap keeps logging in and keeps rendering, and is only
/// refused a new token.
///
/// The two count different things. [`active_token_cap`](Self::active_token_cap)
/// counts rows not yet revoked, so revoking frees one of its slots;
/// [`lifetime_token_cap`](Self::lifetime_token_cap) counts rows ever created,
/// so revoking frees none of its. The first bounds how many engines one
/// account runs at once, the second bounds identity churn.
///
/// Neither is consulted for an account
/// [`administrators`](WebConfig::administrators) names, since the account that
/// issues the tokens this server's own preset engines log in with is not a
/// participant.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountsConfig {
    /// How many of an account's tokens may be unrevoked at once.
    ///
    /// [`NonZeroU32`] rather than a `u32` with a check: a cap of zero is an
    /// account that can never issue a token.
    #[serde(default = "default_active_token_cap")]
    pub active_token_cap: NonZeroU32,

    /// How many tokens an account may ever have been issued, active and revoked
    /// counted together.
    ///
    /// A value below [`active_token_cap`](Self::active_token_cap) is legal and
    /// warned about at startup: the issue path hits this one first, so the
    /// active cap can never bind, which is odd rather than wrong.
    #[serde(default = "default_lifetime_token_cap")]
    pub lifetime_token_cap: NonZeroU32,
}

impl Default for AccountsConfig {
    fn default() -> Self {
        Self {
            active_token_cap: default_active_token_cap(),
            lifetime_token_cap: default_lifetime_token_cap(),
        }
    }
}

/// The `update_interval_seconds` default: fifteen minutes.
///
/// The fit is a batch over two years of rows and costs milliseconds at this
/// server's scale, so the cadence is chosen for the developer waiting on their
/// result rather than for the cost. It divides 86400, as every value of the
/// key must.
fn default_update_interval_seconds() -> NonZeroU64 {
    NonZeroU64::new(900).expect("900 is not zero")
}

/// Seconds in a day, which every rating-update interval must divide.
const SECONDS_IN_A_DAY: u64 = 86_400;

/// Reads `update_interval_seconds`, refusing an interval that does not divide
/// a day.
///
/// The updates run at multiples of the interval measured from UTC midnight, so
/// an interval that did not divide a day would put the last grid point of one
/// day at a different offset from the first of the next.
fn update_interval_seconds<'de, D>(deserializer: D) -> Result<NonZeroU64, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let written = NonZeroU64::deserialize(deserializer)?;
    if !SECONDS_IN_A_DAY.is_multiple_of(written.get()) {
        return Err(D::Error::custom(format!(
            "[ratings].update_interval_seconds {written} does not divide {SECONDS_IN_A_DAY}, the \
             seconds in a day, so the updates could not run on the same wall-clock times every \
             day; write a value {SECONDS_IN_A_DAY} is a multiple of, such as 300, 900, 3600 or \
             {SECONDS_IN_A_DAY}"
        )));
    }

    Ok(written)
}

/// How the two rating tables are updated and where their scale sits.
///
/// The algorithm's five numbers are not among the keys. The half-life of 60
/// days, the 7-day flat period, the 2-year cutoff, the 15-game threshold and
/// the 30-ply disconnect threshold are floodgate's, mirrored one by one, and
/// they live as constants in `services::rating`: an operator who could set the
/// half-life would be running a different algorithm, and their table would not
/// be comparable with anyone else's.
///
/// Neither key here touches the fit. One is operational; the other chooses
/// where the fitted scale's origin sits when nothing else does, which the fit
/// cannot answer at all — the model fixes rating differences, not levels.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RatingsConfig {
    /// How long after one rating update the next one runs.
    ///
    /// [`NonZeroU64`] rather than a `u64` with a check: an interval of zero is
    /// an update that is always due. It must also divide 86400, because the
    /// updates run on a UTC-midnight-aligned grid.
    ///
    /// The first update is not measured by this: it is made at startup, so
    /// that a restart does not report every participant unrated for one
    /// interval.
    #[serde(
        default = "default_update_interval_seconds",
        deserialize_with = "update_interval_seconds"
    )]
    pub update_interval_seconds: NonZeroU64,

    /// What a published table averages when nothing designated is rated yet.
    ///
    /// The fallback origin, and the only one a fresh instance ever uses: until
    /// some engine both carries a designated rating and meets the rated
    /// threshold, there is nothing to place a table against, so the group that
    /// is published is centred on this value instead.
    ///
    /// A plain [`i32`] rather than a `NonZero`: a rating scale has no zero to
    /// avoid and no sign to insist on.
    #[serde(default = "default_fallback_baseline")]
    pub fallback_baseline: i32,
    //
    // There is no key here for an external engine's designated rating. Those
    // are rows in the `designated_ratings` table, written from the admin page,
    // because a designation made while reading a published table should not
    // cost a restart. A file that writes such a key fails at startup, by the
    // `deny_unknown_fields` above.
}

impl Default for RatingsConfig {
    fn default() -> Self {
        Self {
            update_interval_seconds: default_update_interval_seconds(),
            fallback_baseline: default_fallback_baseline(),
        }
    }
}

/// The `fallback_baseline` default.
///
/// 3500, nowhere near floodgate's own scale: a table on this server's fallback
/// origin is not comparable with anybody else's, and a number that looked like
/// a familiar rating would invite exactly that comparison.
const fn default_fallback_baseline() -> i32 {
    3_500
}

impl RatingsConfig {
    /// [`update_interval_seconds`](Self::update_interval_seconds) as a duration.
    pub const fn update_interval(&self) -> Duration {
        Duration::from_secs(self.update_interval_seconds.get())
    }
}

/// The absolute ply limit, and how much of it a game must have left to be worth
/// playing.
///
/// The two keys are one table because neither is usable alone: a
/// `min_playable_plies` with no `max_moves` constrains nothing, and a
/// `max_moves` with no minimum would need a default remainder invented here.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    /// `Max_Moves`, the absolute ply limit — setup moves included, since they
    /// are game history and this limit counts them.
    pub max_moves: u32,

    /// How many plies of real play an entry must leave under `max_moves`.
    pub min_playable_plies: u32,
}

/// The unit every configured time value is counted in.
///
/// A config-local mirror of [`csa::TimeUnit`], since `config` is given no edge
/// to `csa`. The spellings are the wire spellings, so what an operator writes
/// is what `Time_Unit:` carries.
///
/// [`csa::TimeUnit`]: crate::csa::TimeUnit
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum TimeUnit {
    /// `1sec`, the specification's default and this server's.
    #[serde(rename = "1sec")]
    Second,

    /// `1min`.
    #[serde(rename = "1min")]
    Minute,

    /// `1msec`.
    #[serde(rename = "1msec")]
    Millisecond,
}

impl TimeUnit {
    /// `count` of this unit, as a duration. Widening to `u64` before the
    /// multiplication is what makes it total: the largest count is `u32::MAX`
    /// and the largest multiplier 60, and that product fits.
    pub fn duration(self, count: u32) -> Duration {
        let count = u64::from(count);
        match self {
            Self::Second => Duration::from_secs(count),
            Self::Minute => Duration::from_secs(count * 60),
            Self::Millisecond => Duration::from_millis(count),
        }
    }
}

/// The time settings a game is played under.
///
/// The TOML key for [`unit`](Self::unit) is written `time_unit`.
///
/// [`Deserialize`] is hand-written because the numbers cannot be converted
/// field by field: how many seconds `total = 600` means depends on a sibling
/// key. So the raw shape is read first, and the multiplication happens once
/// `time_unit` is known.
///
/// Every key defaults, so the whole table does, to the control floodgate runs:
/// `1sec`, 300 total, a 10-unit increment, no byoyomi, a floor of 0 and no
/// roundup. The defaults are applied before the unit multiplication, so
/// `time_unit = "1min"` alone means 300 minutes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeConfig {
    /// `Time_Unit`, the unit the operator's numbers count.
    pub unit: TimeUnit,

    /// `Total_Time`, the initial allowance. Required: a server with no stated
    /// allowance has nothing to put in the `Time` block.
    pub total: Duration,

    /// `Byoyomi`. Absent means no byoyomi.
    pub byoyomi: Option<Duration>,

    /// `Increment`, added before each turn begins. Defaults to 10 units.
    ///
    /// This is also what a setup move's T-value cancels against: the written T
    /// equals the deduction, so under Fischer increment the two annul and a
    /// client's clock matches the server's at the first real move.
    ///
    /// An [`Option`] because the wire line is optional, but the key's default
    /// is a value, so `increment = 0` is how a configuration says no
    /// increment and omitting the key is how it asks for the default.
    pub increment: Option<Duration>,

    /// `Least_Time_Per_Move`, the floor on a move's charged consumption. Not
    /// optional, because the wire line is always emitted.
    pub least_time_per_move: Duration,

    /// `Time_Roundup`: whether sub-unit consumption rounds up or truncates.
    pub roundup: bool,

    /// Absent for a symmetric game. Its presence sets `TimeCategory`, and it is
    /// what makes two of the three rules in [`validate`] apply at all.
    ///
    /// [`validate`]: mod@super::validate
    pub reduction: Option<Reduction>,
}

/// One side's initial allowance, reduced.
///
/// The reduction never reaches the `Time` block: it rides the setup T-values,
/// landing whole on the reduced side's first setup move, where the value
/// written is the value deducted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reduction {
    /// Whose allowance is reduced.
    pub side: Color,

    /// By how much.
    pub amount: Duration,
}

impl Default for TimeConfig {
    fn default() -> Self {
        Self::of(RawTime::default())
    }
}

impl TimeConfig {
    /// The written table, with every count multiplied by its unit.
    fn of(raw: RawTime) -> Self {
        let unit = raw.time_unit;

        Self {
            unit,
            total: unit.duration(raw.total),
            byoyomi: raw.byoyomi.map(|count| unit.duration(count)),
            increment: raw.increment.map(|count| unit.duration(count)),
            least_time_per_move: unit.duration(raw.least_time_per_move),
            roundup: raw.roundup,
            reduction: raw.reduction.map(|reduction| Reduction {
                side: reduction.side.color(),
                amount: unit.duration(reduction.amount),
            }),
        }
    }
}

impl<'de> Deserialize<'de> for TimeConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        RawTime::deserialize(deserializer).map(Self::of)
    }
}

/// The `[time]` table exactly as written: counts, not durations.
///
/// `u32` because that is what the wire carries, so a number too large to be
/// sent is refused where it is written rather than saturating downstream.
///
/// This is where the `[time]` defaults live, because they are counts of
/// `time_unit` like everything else the operator writes: a table stating the
/// unit alone gets 300 of that unit.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawTime {
    time_unit: TimeUnit,
    total: u32,
    byoyomi: Option<u32>,
    increment: Option<u32>,
    least_time_per_move: u32,
    roundup: bool,
    reduction: Option<RawReduction>,
}

impl Default for RawTime {
    /// The time control floodgate runs, key for key.
    ///
    /// `#[serde(default)]` on the container above takes every missing key from
    /// here, so this one impl is both "the whole table is omitted" and "this key
    /// is omitted" — there is no second list to keep equal to it.
    ///
    /// `increment` is a `Some`, so `[time]` omitted is a Fischer game rather
    /// than a sudden-death one; a configuration wanting no increment writes
    /// `increment = 0`. `byoyomi` is a `None`, because floodgate's control has
    /// none and a byoyomi is not a value to arrive at by omission.
    fn default() -> Self {
        Self {
            time_unit: TimeUnit::Second,
            total: 300,
            byoyomi: None,
            increment: Some(10),
            least_time_per_move: 0,
            roundup: false,
            reduction: None,
        }
    }
}

/// The `[time.reduction]` table exactly as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReduction {
    side: Side,
    amount: u32,
}

/// A side, in the spelling the configuration uses.
///
/// [`Color`] cannot carry this itself: `game/` names nothing outside `std`, so
/// it cannot derive [`Deserialize`].
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Side {
    Black,
    White,
}

impl Side {
    /// The side, as the rules name it.
    fn color(self) -> Color {
        match self {
            Self::Black => Color::Black,
            Self::White => Color::White,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key set, and the time control asymmetric.
    const FULL: &str = "\
auth_mode = \"open\"
positions = \"assets/positions/even.txt\"
records = \"var/records\"
database = \"var/tabia.sqlite3\"

[limit]
max_moves = 512
min_playable_plies = 40

[csa]
host = \"0.0.0.0\"
port = 4081
max_malformed_lines = 8
agreement_timeout_seconds = 30

[time]
time_unit = \"1sec\"
total = 600
increment = 2
least_time_per_move = 0
roundup = false

[time.reduction]
side = \"white\"
amount = 600

[web]
host = \"0.0.0.0\"
port = 8080
";

    /// No `[limit]`, and no key of `[csa]` or `[time]` that the table does not
    /// need stated.
    const MINIMAL: &str = "\
auth_mode = \"github\"
positions = \"positions.txt\"
records = \"records\"
database = \"tabia.sqlite3\"

[csa]
host = \"127.0.0.1\"
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = 600
increment = 0
least_time_per_move = 0
roundup = true
";

    /// The three paths and nothing else: every other key defaults, so this is
    /// the shortest file this server starts from.
    const SHORTEST: &str = "\
positions = \"positions.txt\"
records = \"records\"
database = \"tabia.sqlite3\"
";

    fn parsed(text: &str) -> Config {
        Config::parse(text)
            .unwrap_or_else(|error| panic!("the configuration was rejected: {error}"))
    }

    fn rejected(text: &str) -> String {
        match Config::parse(text) {
            Err(error) => error.to_string(),
            Ok(config) => panic!("the configuration parsed to {config:?}"),
        }
    }

    /// `MINIMAL` with one line replaced.
    fn with_line(replaced: &str, replacement: &str) -> String {
        assert!(MINIMAL.contains(replaced), "{replaced} is not in MINIMAL");
        MINIMAL.replace(replaced, replacement)
    }

    #[test]
    fn the_documented_layout_parses_field_by_field() {
        let config = parsed(FULL);

        assert_eq!(config.auth_mode, AuthMode::Open);
        assert_eq!(config.positions, PathBuf::from("assets/positions/even.txt"));
        assert_eq!(config.records, PathBuf::from("var/records"));
        assert_eq!(
            config.limit,
            Some(Limit {
                max_moves: 512,
                min_playable_plies: 40,
            })
        );
        assert_eq!(config.time.unit, TimeUnit::Second);
        assert_eq!(config.time.total, Duration::from_secs(600));
        assert_eq!(config.time.byoyomi, None);
        assert_eq!(config.time.increment, Some(Duration::from_secs(2)));
        assert_eq!(config.time.least_time_per_move, Duration::ZERO);
        assert!(!config.time.roundup);
        assert_eq!(
            config.time.reduction,
            Some(Reduction {
                side: Color::White,
                amount: Duration::from_secs(600),
            })
        );
        assert_eq!(config.csa.listen(), "0.0.0.0:4081");
        assert_eq!(config.csa.max_malformed_lines.get(), 8);
        assert_eq!(config.csa.agreement_timeout(), Duration::from_secs(30));
        assert_eq!(
            config.web,
            WebConfig {
                host: "0.0.0.0".to_owned(),
                port: 8080,
                oauth: None,
                administrators: Vec::new(),
            }
        );
        assert_eq!(config.web.listen(), "0.0.0.0:8080");
    }

    #[test]
    fn an_unstated_agreement_timeout_is_120_seconds() {
        assert_eq!(
            parsed(MINIMAL).csa.agreement_timeout(),
            crate::session::AGREEMENT_TIMEOUT
        );
    }

    #[test]
    fn the_three_paths_are_the_whole_of_what_a_file_must_state() {
        // Everything else defaults, so this file starts a server.
        let config = parsed(SHORTEST);

        assert_eq!(config.auth_mode, AuthMode::Open);
        assert_eq!(config.csa, CsaConfig::default());
        assert_eq!(config.time, TimeConfig::default());
        assert_eq!(config.limit, None);
        assert_eq!(config.web, WebConfig::default());
    }

    #[test]
    fn a_required_top_level_key_may_not_be_omitted() {
        // The three paths, and only the three.
        for key in ["positions", "records", "database"] {
            let text: String = MINIMAL
                .lines()
                .filter(|line| !line.starts_with(key))
                .collect::<Vec<_>>()
                .join("\n");

            let message = rejected(&text);
            assert!(message.contains(key), "{key}: {message}");
        }
    }

    #[test]
    fn an_omitted_auth_mode_is_the_open_mode_an_instance_can_serve_alone() {
        // `github` verifies against tokens only a browser sign-in issues, so it
        // is not a mode to reach by leaving a key out.
        let text: String = MINIMAL
            .lines()
            .filter(|line| !line.starts_with("auth_mode"))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(parsed(&text).auth_mode, AuthMode::Open);
    }

    #[test]
    fn an_omitted_csa_table_binds_the_port_floodgate_listens_on() {
        // Every key of the table has a default, and an empty table says the
        // same as an absent one.
        let omitted = parsed(SHORTEST).csa;

        assert_eq!(omitted.listen(), "0.0.0.0:4081");
        assert_eq!(omitted.max_malformed_lines.get(), 8);
        assert_eq!(omitted.agreement_timeout(), Duration::from_secs(120));
        assert_eq!(omitted.tls, None);

        assert_eq!(parsed(&format!("{SHORTEST}\n[csa]\n")).csa, omitted);
    }

    #[test]
    fn one_stated_csa_key_leaves_the_others_at_their_defaults() {
        let csa = parsed(&format!("{SHORTEST}\n[csa]\nport = 4444\n")).csa;

        assert_eq!(csa.listen(), "0.0.0.0:4444");
        assert_eq!(csa.max_malformed_lines.get(), 8);
    }

    #[test]
    fn an_ipv6_host_is_written_bare_and_bracketed_where_it_is_joined() {
        // The split into two keys leaves an operator nowhere to put the
        // brackets an address needs.
        let text =
            format!("{SHORTEST}\n[csa]\nhost = \"::1\"\nport = 4081\n\n[web]\nhost = \"::\"\n");
        let config = parsed(&text);

        assert_eq!(config.csa.listen(), "[::1]:4081");
        assert_eq!(config.web.listen(), "[::]:8080");
    }

    #[test]
    fn a_port_outside_the_range_a_port_has_is_refused_where_it_is_written() {
        for (table, value) in [("csa", "65536"), ("web", "-1"), ("csa", "\"4081\"")] {
            let message = rejected(&format!("{SHORTEST}\n[{table}]\nport = {value}\n"));

            assert!(message.contains("port"), "{table} {value}: {message}");
        }
    }

    #[test]
    fn a_listen_key_on_either_table_is_refused_by_name() {
        // `listen` is not a key on either table.
        for table in ["csa", "web"] {
            let message = rejected(&format!(
                "{SHORTEST}\n[{table}]\nlisten = \"127.0.0.1:4081\"\n"
            ));

            assert!(message.contains("unknown"), "{table}: {message}");
            assert!(message.contains("listen"), "{table}: {message}");
        }
    }

    #[test]
    fn a_server_table_is_refused_by_name() {
        // There is no `[server]` table and no alias for one: the CSA listener
        // is configured through `[csa]`.
        for table in ["[server]\nport = 4081\n", "[server.tls]\ncert = \"c\"\n"] {
            let message = rejected(&format!("{SHORTEST}\n{table}"));

            assert!(message.contains("unknown"), "{table}: {message}");
            assert!(message.contains("server"), "{table}: {message}");
        }
    }

    #[test]
    fn a_malformed_line_limit_of_zero_is_refused_where_it_is_written() {
        // A limit of zero would close every connection before its first line.
        let message = rejected(&with_line(
            "max_malformed_lines = 4",
            "max_malformed_lines = 0",
        ));

        assert!(message.contains('0'), "{message}");
    }

    #[test]
    fn an_unknown_csa_key_is_rejected_by_name() {
        let message = rejected(&with_line(
            "max_malformed_lines = 4",
            "max_malformed_lines = 4\nquickack = true",
        ));

        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("quickack"), "{message}");
    }

    #[test]
    fn no_tls_table_leaves_the_listener_plaintext() {
        assert_eq!(parsed(MINIMAL).csa.tls, None);
    }

    #[test]
    fn a_tls_table_carries_the_two_pem_paths() {
        let text = format!(
            "{MINIMAL}\n[csa.tls]\ncert = \"/etc/tabia/cert.pem\"\nkey = \"/etc/tabia/key.pem\"\n"
        );

        assert_eq!(
            parsed(&text).csa.tls,
            Some(TlsConfig {
                cert: PathBuf::from("/etc/tabia/cert.pem"),
                key: PathBuf::from("/etc/tabia/key.pem"),
            })
        );
    }

    #[test]
    fn a_half_written_tls_table_names_the_key_that_is_missing() {
        for (table, missing) in [
            ("[csa.tls]\ncert = \"cert.pem\"\n", "key"),
            ("[csa.tls]\nkey = \"key.pem\"\n", "cert"),
        ] {
            let message = rejected(&format!("{MINIMAL}\n{table}"));
            assert!(message.contains(missing), "{missing}: {message}");
        }
    }

    #[test]
    fn an_omitted_web_table_is_the_same_listener_an_empty_one_is() {
        // The HTTP listener always runs, so absence is not an off switch.
        let omitted = parsed(MINIMAL).web;

        assert_eq!(omitted, WebConfig::default());
        assert_eq!(parsed(&format!("{MINIMAL}\n[web]\n")).web, omitted);
    }

    #[test]
    fn a_web_table_carries_the_address_the_http_listener_binds() {
        let text = format!("{MINIMAL}\n[web]\nhost = \"0.0.0.0\"\n");

        assert_eq!(
            parsed(&text).web,
            WebConfig {
                host: "0.0.0.0".to_owned(),
                port: 8080,
                oauth: None,
                administrators: Vec::new(),
            }
        );
    }

    #[test]
    fn an_unstated_web_address_is_the_loopback_default() {
        // Loopback rather than every interface, because this half is plaintext.
        assert_eq!(
            parsed(&format!("{SHORTEST}\n[web]\n")).web.listen(),
            "127.0.0.1:8080"
        );
    }

    #[test]
    fn a_web_oauth_table_carries_the_client_id_the_authorize_url_names() {
        // The one configured value: the client id is public, and the client
        // secret and the cookie signing key are read from the environment.
        let text = format!(
            "{MINIMAL}\n[web]\nhost = \"0.0.0.0\"\n\n[web.oauth]\nclient_id = \"Iv23liXX\"\n"
        );

        assert_eq!(
            parsed(&text).web,
            WebConfig {
                host: "0.0.0.0".to_owned(),
                port: 8080,
                oauth: Some(OauthConfig {
                    client_id: "Iv23liXX".to_owned(),
                }),
                administrators: Vec::new(),
            }
        );
    }

    #[test]
    fn the_administrators_are_github_user_ids_and_default_to_nobody() {
        // Absent is the shipped configuration: no account administers the
        // instance.
        assert_eq!(
            parsed(&format!("{MINIMAL}\n[web]\nhost = \"0.0.0.0\"\n"))
                .web
                .administrators,
            Vec::<i64>::new()
        );

        assert_eq!(
            parsed(&format!(
                "{MINIMAL}\n[web]\nhost = \"0.0.0.0\"\nadministrators = [4242, 7]\n"
            ))
            .web
            .administrators,
            [4_242, 7]
        );
    }

    #[test]
    fn a_web_oauth_table_names_the_keys_it_wants_and_refuses_the_ones_it_does_not_have() {
        // The table written empty has not named an app.
        let empty = rejected(&format!(
            "{MINIMAL}\n[web]\nhost = \"0.0.0.0\"\n\n[web.oauth]\n"
        ));
        assert!(empty.contains("client_id"), "{empty}");

        // And a secret written into the file is refused where it is written:
        // the client secret comes from the environment, never from this file.
        let secret = rejected(&format!(
            "{MINIMAL}\n[web]\nhost = \"0.0.0.0\"\n\n\
             [web.oauth]\nclient_id = \"Iv23liXX\"\nclient_secret = \"shh\"\n"
        ));
        assert!(secret.contains("client_secret"), "{secret}");
    }

    #[test]
    fn an_unknown_web_key_is_rejected_by_name() {
        let text = format!("{MINIMAL}\n[web]\nhost = \"0.0.0.0\"\nbase_url = \"/\"\n");

        let message = rejected(&text);

        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("base_url"), "{message}");
    }

    #[test]
    fn an_unknown_tls_key_is_rejected_by_name() {
        let text = format!(
            "{MINIMAL}\n[csa.tls]\ncert = \"cert.pem\"\nkey = \"key.pem\"\nciphers = \"any\"\n"
        );

        let message = rejected(&text);

        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("ciphers"), "{message}");
    }

    #[test]
    fn a_bare_tls_boolean_is_refused() {
        // The switch is the table's presence, so `tls = true` is not a way to
        // turn TLS on with no material behind it.
        let message = rejected(&with_line(
            "max_malformed_lines = 4",
            "max_malformed_lines = 4\ntls = true",
        ));

        assert!(message.contains("tls"), "{message}");
    }

    #[test]
    fn both_authentication_modes_parse_and_a_third_is_rejected_by_name() {
        assert_eq!(parsed(MINIMAL).auth_mode, AuthMode::Github);
        assert_eq!(
            parsed(&with_line("\"github\"", "\"open\"")).auth_mode,
            AuthMode::Open
        );

        let message = rejected(&with_line("\"github\"", "\"gitlab\""));
        assert!(message.contains("gitlab"), "{message}");
        assert!(message.contains("github"), "{message}");
        assert!(message.contains("open"), "{message}");
    }

    #[test]
    fn a_time_value_is_a_count_of_the_configured_unit() {
        // The operator writes the numbers that go on the wire.
        for (unit, spelling, total) in [
            (TimeUnit::Second, "\"1sec\"", Duration::from_secs(600)),
            (TimeUnit::Minute, "\"1min\"", Duration::from_secs(600 * 60)),
            (
                TimeUnit::Millisecond,
                "\"1msec\"",
                Duration::from_millis(600),
            ),
        ] {
            let config = parsed(&with_line("\"1sec\"", spelling));

            assert_eq!(config.time.unit, unit, "{spelling}");
            assert_eq!(config.time.total, total, "{spelling}");
        }
    }

    #[test]
    fn every_time_value_is_multiplied_by_the_unit_not_only_the_total() {
        let text = "\
auth_mode = \"open\"
positions = \"positions.txt\"
records = \"records\"
database = \"tabia.sqlite3\"

[csa]
host = \"127.0.0.1\"
max_malformed_lines = 4

[time]
time_unit = \"1min\"
total = 10
byoyomi = 1
increment = 2
least_time_per_move = 3
roundup = false

[time.reduction]
side = \"black\"
amount = 5
";

        let time = parsed(text).time;

        assert_eq!(time.total, Duration::from_secs(600));
        assert_eq!(time.byoyomi, Some(Duration::from_secs(60)));
        assert_eq!(time.increment, Some(Duration::from_secs(120)));
        assert_eq!(time.least_time_per_move, Duration::from_secs(180));
        assert_eq!(
            time.reduction,
            Some(Reduction {
                side: Color::Black,
                amount: Duration::from_secs(300),
            })
        );
    }

    #[test]
    fn an_omitted_byoyomi_is_absent_rather_than_zero() {
        // The one `[time]` key whose default is an absence: floodgate's control
        // has no byoyomi.
        let time = parsed(MINIMAL).time;

        assert_eq!(time.byoyomi, None);
        assert_eq!(time.increment, Some(Duration::ZERO));
        assert_eq!(time.reduction, None);
        assert_eq!(time.least_time_per_move, Duration::ZERO);
    }

    #[test]
    fn an_omitted_time_table_is_the_control_floodgate_runs() {
        let time = parsed(SHORTEST).time;

        assert_eq!(time, TimeConfig::default());
        assert_eq!(time.unit, TimeUnit::Second);
        assert_eq!(time.total, Duration::from_secs(300));
        assert_eq!(time.increment, Some(Duration::from_secs(10)));
        assert_eq!(time.byoyomi, None);
        assert_eq!(time.least_time_per_move, Duration::ZERO);
        assert!(!time.roundup);
        assert_eq!(time.reduction, None);

        // And an empty table says the same thing as an absent one.
        assert_eq!(parsed(&format!("{SHORTEST}\n[time]\n")).time, time);
    }

    #[test]
    fn one_stated_time_key_leaves_the_others_at_their_defaults() {
        // Including the unit, so a table stating `1min` alone means 300
        // minutes.
        let stated = parsed(&format!("{SHORTEST}\n[time]\ntotal = 60\n")).time;
        assert_eq!(stated.total, Duration::from_secs(60));
        assert_eq!(stated.increment, Some(Duration::from_secs(10)));

        let minutes = parsed(&format!("{SHORTEST}\n[time]\ntime_unit = \"1min\"\n")).time;
        assert_eq!(minutes.total, Duration::from_secs(300 * 60));
        assert_eq!(minutes.increment, Some(Duration::from_secs(10 * 60)));
    }

    #[test]
    fn an_absent_limit_table_is_none_and_a_half_written_one_is_an_error() {
        assert_eq!(parsed(MINIMAL).limit, None);

        for (table, missing) in [
            ("[limit]\nmax_moves = 512\n", "min_playable_plies"),
            ("[limit]\nmin_playable_plies = 40\n", "max_moves"),
        ] {
            let message = rejected(&format!("{MINIMAL}\n{table}"));
            assert!(message.contains(missing), "{message}");
        }
    }

    #[test]
    fn an_unknown_key_is_rejected_at_every_level() {
        let cases = [
            // Before the first table header, so this one is at the top level;
            // everything appended lands inside `[time]`.
            (format!("max_games = 4\n{MINIMAL}"), "max_games"),
            (
                format!(
                    "{MINIMAL}\n[limit]\nmax_moves = 512\nmin_playable_plies = 40\nmax_plies = 9\n"
                ),
                "max_plies",
            ),
            (format!("{MINIMAL}byoyomi_stock = 3\n"), "byoyomi_stock"),
            (
                format!(
                    "{MINIMAL}\n[time.reduction]\nside = \"black\"\namount = 60\nspread = true\n"
                ),
                "spread",
            ),
        ];

        for (text, offender) in cases {
            let message = rejected(&text);
            assert!(message.contains("unknown"), "{offender}: {message}");
            assert!(message.contains(offender), "{offender}: {message}");
        }
    }

    #[test]
    fn an_unknown_time_unit_is_rejected_by_name_listing_the_three_accepted() {
        let message = rejected(&with_line("\"1sec\"", "\"200msec\""));

        assert!(message.contains("200msec"), "{message}");
        for accepted in ["1sec", "1min", "1msec"] {
            assert!(message.contains(accepted), "{accepted}: {message}");
        }
    }

    #[test]
    fn an_unknown_reduced_side_is_rejected_by_name() {
        let text = format!("{MINIMAL}\n[time.reduction]\nside = \"sente\"\namount = 60\n");

        let message = rejected(&text);

        assert!(message.contains("sente"), "{message}");
        assert!(message.contains("black"), "{message}");
        assert!(message.contains("white"), "{message}");
    }

    #[test]
    fn a_time_count_outside_the_wire_range_is_rejected_where_it_is_written() {
        // u32 is what the wire carries, so both ends are refused here.
        for count in ["4294967296", "-1"] {
            let message = rejected(&with_line("total = 600", &format!("total = {count}")));
            assert!(message.contains(count), "{count}: {message}");
        }
    }

    /// `MINIMAL` with a `[matchmaking]` table appended last, which keeps the
    /// appended keys out of `[time]`.
    fn with_matchmaking(table: &str) -> String {
        format!("{MINIMAL}\n[matchmaking]\n{table}")
    }

    #[test]
    fn an_unstated_matchmaking_table_is_the_documented_defaults() {
        let matchmaking = parsed(MINIMAL).matchmaking;

        assert_eq!(matchmaking, MatchmakingConfig::default());
        assert_eq!(matchmaking.idle_delay(), Duration::from_secs(60));
        assert_eq!(matchmaking.interval(), Duration::from_secs(1_800));
        assert_eq!(matchmaking.first_round_at, None);
    }

    #[test]
    fn the_three_schedule_keys_parse_as_written() {
        let matchmaking = parsed(&with_matchmaking(
            "idle_delay_seconds = 0\n\
             interval_seconds = 1\n\
             first_round_at = \"2026-11-14T09:00:00+09:00\"\n",
        ))
        .matchmaking;

        assert_eq!(matchmaking.idle_delay(), Duration::ZERO);
        assert_eq!(matchmaking.interval(), Duration::from_secs(1));
        let first = matchmaking
            .first_round_at
            .expect("the timestamp is written");
        assert_eq!(first.to_string(), "2026-11-14T09:00:00+09:00");
        assert_eq!(
            first.at(),
            crate::config::timestamp::parse("2026-11-14T00:00:00Z").expect("the same moment"),
        );
    }

    #[test]
    fn one_stated_schedule_key_leaves_the_others_at_their_defaults() {
        let matchmaking = parsed(&with_matchmaking("interval_seconds = 60\n")).matchmaking;

        assert_eq!(matchmaking.interval(), Duration::from_secs(60));
        assert_eq!(matchmaking.idle_delay(), Duration::from_secs(60));
    }

    #[test]
    fn an_unstated_unrated_estimate_is_the_matchmakers_own_default() {
        // `config` does not depend on `session`, so nothing but this test keeps
        // a change to one of the two spellings from leaving the other behind.
        let matchmaking = parsed(MINIMAL).matchmaking;

        assert_eq!(matchmaking.unrated_estimate, 3_000);
        assert_eq!(
            matchmaking.unrated_estimate,
            crate::session::matchmaker::DEFAULT_RATE
        );
    }

    #[test]
    fn an_unrated_estimate_is_read_as_written_at_any_magnitude() {
        // A plain `i32`: every one of these is a scale an operator may run.
        for written in [0, -250, 1_500, 3_000, 9_999] {
            let matchmaking = parsed(&with_matchmaking(&format!(
                "unrated_estimate = {written}\n"
            )))
            .matchmaking;

            assert_eq!(matchmaking.unrated_estimate, written);
        }
    }

    #[test]
    fn an_empty_first_round_at_says_the_same_as_no_first_round_at() {
        assert_eq!(
            parsed(&with_matchmaking("first_round_at = \"\"\n"))
                .matchmaking
                .first_round_at,
            None,
        );
    }

    #[test]
    fn a_first_round_at_that_is_not_rfc_3339_is_refused_naming_the_value() {
        for written in ["2026-11-14 09:00:00", "2026-11-14T09:00:00", "tomorrow"] {
            let message = rejected(&with_matchmaking(&format!(
                "first_round_at = \"{written}\"\n"
            )));

            assert!(message.contains(written), "{written}: {message}");
        }
    }

    #[test]
    fn a_schedule_value_outside_its_domain_is_refused_where_it_is_written() {
        // An interval of zero is a round that is always due, and a negative
        // delay is not a delay.
        for (key, value) in [
            ("interval_seconds", "0"),
            ("interval_seconds", "-1"),
            ("idle_delay_seconds", "-1"),
            ("idle_delay_seconds", "1.5"),
            ("interval_seconds", "\"1800\""),
        ] {
            let message = rejected(&with_matchmaking(&format!("{key} = {value}\n")));

            assert!(message.contains(key), "{key} = {value}: {message}");
        }
    }

    #[test]
    fn an_unknown_matchmaking_key_is_rejected_by_name() {
        let message = rejected(&with_matchmaking("round_seconds = 60\n"));

        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("round_seconds"), "{message}");
    }

    /// A CSA engine the operator runs, which is the shape a bare `token` has.
    fn preset(token: &str) -> String {
        format!("{{ token = \"{token}\" }}")
    }

    /// One USI preset entry, with the two keys that protocol requires.
    fn usi_preset(token: &str) -> String {
        format!(
            "{{ token = \"{token}\", protocol = \"usi\", command = [\"/opt/preset\"], \
               lifecycle = \"on-demand\" }}"
        )
    }

    #[test]
    fn an_unstated_preset_engine_list_designates_nobody() {
        let presets = parsed(MINIMAL).matchmaking.preset_engine_tokens;

        assert!(presets.is_empty());
        assert_eq!(presets.len(), 0);
        assert_eq!(presets.designates("anything"), None);
    }

    #[test]
    fn an_empty_preset_engine_list_says_the_same_as_no_list() {
        assert_eq!(
            parsed(&with_matchmaking("preset_engine_tokens = []\n"))
                .matchmaking
                .preset_engine_tokens,
            PresetEngines::default(),
        );
    }

    #[test]
    fn a_designated_token_is_matched_byte_for_byte() {
        let presets = parsed(&with_matchmaking(&format!(
            "preset_engine_tokens = [{}, {}]\n",
            preset("house-1"),
            preset("house-2")
        )))
        .matchmaking
        .preset_engine_tokens;

        assert_eq!(presets.len(), 2);
        assert_eq!(presets.designates("house-1"), Some(0));
        assert_eq!(presets.designates("house-2"), Some(1));

        // Case-sensitive, and nothing is trimmed.
        assert_eq!(presets.designates("House-1"), None);
        assert_eq!(presets.designates(" house-1"), None);
        assert_eq!(presets.designates("house-1 "), None);
        assert_eq!(presets.designates("house-10"), None);
        assert_eq!(presets.designates(""), None);
    }

    #[test]
    fn a_usi_preset_carries_everything_the_server_needs_to_run_it() {
        let presets = parsed(&with_matchmaking(
            "preset_engine_tokens = [\n  \
               { token = \"undesignated\", protocol = \"usi\", \
                 command = [\"/opt/run\", \"--engine\", \"a\"], lifecycle = \"on-demand\", \
                 usi_options = { USI_Hash = 256, USI_Ponder = false, BookFile = \"/opt/b.bin\" }, \
                 name = \"engine-a\" },\n  \
               { token = \"reference\", protocol = \"usi\", command = [\"/opt/run\"], \
                 lifecycle = \"resident\", rating = 1800 },\n\
             ]\n",
        ))
        .matchmaking
        .preset_engine_tokens;

        let undesignated = presets.get(0).expect("the first entry parsed");
        assert_eq!(undesignated.command, ["/opt/run", "--engine", "a"]);
        assert_eq!(undesignated.rating, None);
        assert!(!undesignated.has_designated_rating());
        assert_eq!(presets.designated_rating(0), None);
        assert!(!undesignated.is_externally_run());
        assert!(!presets.is_externally_run(0));
        assert_eq!(presets.lifecycle(0), Some(Lifecycle::OnDemand));
        assert_eq!(undesignated.name.as_deref(), Some("engine-a"));

        // The option names' own order, not the file's.
        assert_eq!(
            undesignated.usi_options.iter().collect::<Vec<_>>(),
            [
                ("BookFile", "/opt/b.bin".to_owned()),
                ("USI_Hash", "256".to_owned()),
                ("USI_Ponder", "false".to_owned()),
            ],
        );

        let reference = presets.get(1).expect("the second entry parsed");
        assert_eq!(reference.rating, Some(1_800));
        assert!(reference.has_designated_rating());
        assert_eq!(presets.designated_rating(1), Some(1_800));
        assert_eq!(presets.lifecycle(1), Some(Lifecycle::Resident));
        assert!(reference.usi_options.is_empty());
        assert_eq!(reference.name, None);

        // An index this list has no entry at is not a preset.
        assert_eq!(presets.get(2), None);
        assert_eq!(presets.designated_rating(2), None);
        assert_eq!(presets.lifecycle(2), None);
        assert!(presets.is_externally_run(2));
    }

    #[test]
    fn a_csa_preset_is_one_the_server_does_not_run() {
        // The protocol may be omitted altogether or written out, and the two
        // say the same thing.
        for written in ["", ", protocol = \"csa\""] {
            let presets = parsed(&with_matchmaking(&format!(
                "preset_engine_tokens = [\n  \
                   {{ token = \"run-by-the-operator\"{written}, rating = 1800 }},\n  \
                   {{ token = \"run-by-the-server\", protocol = \"usi\", \
                      command = [\"/opt/run\"], lifecycle = \"on-demand\" }},\n\
                 ]\n"
            )))
            .matchmaking
            .preset_engine_tokens;

            // Still a registered preset in every other respect.
            assert_eq!(presets.designates("run-by-the-operator"), Some(0));
            assert_eq!(presets.designated_rating(0), Some(1_800));

            assert!(presets.is_externally_run(0), "{written:?}");
            assert_eq!(presets.lifecycle(0), None, "{written:?}");
            assert!(!presets.is_externally_run(1), "{written:?}");
        }
    }

    #[test]
    fn a_key_the_protocol_has_no_use_for_is_refused_naming_it() {
        // A `command` on a CSA entry is a command nobody would ever run, and an
        // operator cannot see that from the outside.
        for key in [
            "command = [\"/opt/run\"]",
            "lifecycle = \"resident\"",
            "usi_options = { USI_Hash = 256 }",
            "name = \"engine-a\"",
        ] {
            let message = rejected(&with_matchmaking(&format!(
                "preset_engine_tokens = [{}, {{ token = \"csa-2\", {key} }}]\n",
                preset("csa-1"),
            )));

            let named = key.split(' ').next().expect("the key is the first word");
            assert!(message.contains("entry 2"), "{key}: {message}");
            assert!(message.contains(named), "{key}: {message}");
        }
    }

    #[test]
    fn a_usi_entry_missing_a_key_it_requires_is_refused_naming_it() {
        for (missing, written) in [
            ("command", "lifecycle = \"resident\""),
            ("lifecycle", "command = [\"/opt/run\"]"),
        ] {
            let message = rejected(&with_matchmaking(&format!(
                "preset_engine_tokens = [{}, \
                 {{ token = \"usi-2\", protocol = \"usi\", {written} }}]\n",
                preset("csa-1"),
            )));

            assert!(message.contains("entry 2"), "{missing}: {message}");
            assert!(message.contains(missing), "{missing}: {message}");
        }
    }

    #[test]
    fn an_unknown_protocol_or_lifecycle_is_refused() {
        for written in [
            "{ token = \"a\", protocol = \"uci\" }",
            "{ token = \"a\", protocol = \"usi\", command = [\"/opt/run\"], \
               lifecycle = \"always\" }",
            "{ token = \"a\", protocol = \"usi\", command = [\"/opt/run\"], \
               lifecycle = \"on_demand\" }",
        ] {
            let message = rejected(&with_matchmaking(&format!(
                "preset_engine_tokens = [{written}]\n"
            )));

            assert!(message.contains("preset_engine_tokens"), "{written}");
            assert!(!message.is_empty(), "{written}");
        }
    }

    #[test]
    fn a_token_no_client_could_present_is_refused_naming_its_position() {
        let long = "t".repeat(MAX_PRESET_ENGINE_TOKEN_LEN + 1);
        for unusable in ["", "with space", "tok'en", &long, "エンジン"] {
            let message = rejected(&with_matchmaking(&format!(
                "preset_engine_tokens = [{}, {}]\n",
                preset("ok-token"),
                preset(unusable)
            )));

            assert!(message.contains("preset_engine_tokens"), "{message}");
            // The offending entry is named by its position.
            assert!(message.contains("entry 2"), "{unusable:?}: {message}");
        }
    }

    #[test]
    fn a_command_with_a_blank_word_is_refused_naming_its_position() {
        // A list with a blank word in it names no program and no argument.
        for command in ["[\"\"]", "[\"/opt/run\", \"\"]"] {
            let message = rejected(&with_matchmaking(&format!(
                "preset_engine_tokens = [{}, {{ token = \"house-2\", protocol = \"usi\", \
                   lifecycle = \"on-demand\", command = {command} }}]\n",
                preset("house-1")
            )));

            assert!(message.contains("preset_engine_tokens"), "{message}");
            assert!(message.contains("entry 2"), "{command}: {message}");
        }
    }

    #[test]
    fn a_preset_entry_rejects_a_key_it_does_not_have() {
        let message = rejected(&with_matchmaking(
            "preset_engine_tokens = [{ token = \"a\", ratings = 1800 }]\n",
        ));

        assert!(message.contains("ratings"), "{message}");
    }

    #[test]
    fn a_token_at_the_bounds_of_the_charset_is_accepted() {
        let longest = "t".repeat(MAX_PRESET_ENGINE_TOKEN_LEN);
        let presets = parsed(&with_matchmaking(&format!(
            "preset_engine_tokens = [{}, {}]\n",
            preset("!~"),
            preset(&longest)
        )))
        .matchmaking
        .preset_engine_tokens;

        assert_eq!(presets.designates("!~"), Some(0));
        assert_eq!(presets.designates(&longest), Some(1));
    }

    #[test]
    fn a_repeated_token_is_refused_naming_both_positions() {
        // Across the two protocols as well as within one.
        let message = rejected(&with_matchmaking(&format!(
            "preset_engine_tokens = [{}, {}, {}]\n",
            preset("one"),
            preset("two"),
            usi_preset("one")
        )));

        assert!(message.contains("entry 3"), "{message}");
        assert!(message.contains("entry 1"), "{message}");
    }

    #[test]
    fn the_token_length_bound_is_the_one_the_wire_enforces() {
        // `config` does not depend on `csa`, so nothing but this test keeps a
        // change to one of the two spellings from leaving the other behind.
        assert_eq!(
            MAX_PRESET_ENGINE_TOKEN_LEN,
            crate::csa::command::MAX_TOKEN_LEN
        );
    }

    #[test]
    fn a_designated_token_never_reaches_a_debug_line() {
        let written = "preset_engine_tokens = [{ token = \"s3cret-house-engine\", \
                       protocol = \"usi\", lifecycle = \"resident\", \
                       command = [\"/opt/run\", \"--engine\", \"s3cret-house-engine\"] }]\n";
        let presets = parsed(&with_matchmaking(written))
            .matchmaking
            .preset_engine_tokens;

        let printed = format!("{presets:?}");
        assert!(!printed.contains("s3cret"), "{printed}");
        assert!(printed.contains('1'), "{printed}");

        // Including the copy of it the operator's own command line carries.
        let entry = format!("{:?}", presets.get(0).expect("one entry"));
        assert!(!entry.contains("s3cret"), "{entry}");
        assert!(entry.contains("/opt/run"), "{entry}");

        // The whole configuration derives `Debug`, so the field is what has to
        // stay quiet.
        let whole = format!("{:?}", parsed(&with_matchmaking(written)));
        assert!(!whole.contains("s3cret"), "{whole}");

        // A CSA entry says which kind of entry it is rather than printing a
        // blank.
        let externally_run = parsed(&with_matchmaking(
            "preset_engine_tokens = [{ token = \"s3cret-outside-engine\" }]\n",
        ))
        .matchmaking
        .preset_engine_tokens;
        let entry = format!("{:?}", externally_run.get(0).expect("one entry"));
        assert!(!entry.contains("s3cret"), "{entry}");
        assert!(entry.contains("externally run"), "{entry}");
    }

    /// `MINIMAL` with an `[accounts]` table appended.
    fn with_accounts(table: &str) -> String {
        format!("{MINIMAL}\n[accounts]\n{table}")
    }

    #[test]
    fn an_unstated_accounts_table_is_the_default_caps() {
        // The two default caps: 3 active, 16 ever issued.
        let accounts = parsed(MINIMAL).accounts;

        assert_eq!(accounts, AccountsConfig::default());
        assert_eq!(accounts.active_token_cap.get(), 3);
        assert_eq!(accounts.lifetime_token_cap.get(), 16);
    }

    #[test]
    fn both_caps_parse_as_written_and_one_leaves_the_other_at_its_default() {
        let accounts = parsed(&with_accounts(
            "active_token_cap = 5\nlifetime_token_cap = 40\n",
        ))
        .accounts;
        assert_eq!(accounts.active_token_cap.get(), 5);
        assert_eq!(accounts.lifetime_token_cap.get(), 40);

        let one = parsed(&with_accounts("active_token_cap = 1\n")).accounts;
        assert_eq!(one.active_token_cap.get(), 1);
        assert_eq!(one.lifetime_token_cap.get(), 16);
    }

    #[test]
    fn a_cap_of_zero_is_refused_where_it_is_written() {
        // A cap of zero is an account that can never issue a token.
        for key in ["active_token_cap", "lifetime_token_cap"] {
            let message = rejected(&with_accounts(&format!("{key} = 0\n")));
            assert!(message.contains(key), "{key}: {message}");
        }
    }

    #[test]
    fn a_cap_outside_its_domain_is_refused_where_it_is_written() {
        for (key, value) in [
            ("active_token_cap", "-1"),
            ("lifetime_token_cap", "1.5"),
            ("active_token_cap", "\"3\""),
        ] {
            let message = rejected(&with_accounts(&format!("{key} = {value}\n")));
            assert!(message.contains(key), "{key} = {value}: {message}");
        }
    }

    #[test]
    fn an_unknown_accounts_key_is_rejected_by_name() {
        let message = rejected(&with_accounts("token_cap = 3\n"));

        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("token_cap"), "{message}");
    }

    /// `MINIMAL` with a `[ratings]` table appended.
    fn with_ratings(table: &str) -> String {
        format!("{MINIMAL}\n[ratings]\n{table}")
    }

    #[test]
    fn an_unstated_ratings_table_updates_every_fifteen_minutes() {
        let ratings = parsed(MINIMAL).ratings;

        assert_eq!(ratings, RatingsConfig::default());
        assert_eq!(ratings.update_interval_seconds.get(), 900);
        assert_eq!(ratings.update_interval(), Duration::from_secs(900));
        // The default is on the grid it asks every value to be on: 96 a day.
        assert_eq!(86_400 % ratings.update_interval_seconds.get(), 0);

        // And an empty table says the same thing as an absent one.
        assert_eq!(parsed(&with_ratings("")).ratings, ratings);
    }

    #[test]
    fn an_unstated_ratings_table_falls_back_to_thirty_five_hundred() {
        // The shipped configuration: nothing is designated anywhere, so every
        // table takes the fallback origin.
        let ratings = parsed(MINIMAL).ratings;

        assert_eq!(ratings.fallback_baseline, 3_500);

        // And an empty table says the same thing as an absent one.
        assert_eq!(parsed(&with_ratings("")).ratings, ratings);
    }

    #[test]
    fn a_fallback_baseline_is_read_as_written_at_any_magnitude() {
        // A rating scale has no zero to avoid and no sign to insist on.
        for written in [0, -250, 1_000, 3_500, 9_999] {
            let ratings =
                parsed(&with_ratings(&format!("fallback_baseline = {written}\n"))).ratings;

            assert_eq!(ratings.fallback_baseline, written);
        }
    }

    #[test]
    fn a_designation_under_ratings_fails_naming_the_key() {
        // Designations are rows, not a key under `[ratings]`.
        let message = rejected(&with_ratings(&format!(
            "designated_ratings = [{{ participant = \"{}\", rating = 2400 }}]\n",
            "f".repeat(64)
        )));

        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("designated_ratings"), "{message}");
    }

    #[test]
    fn the_update_cadence_parses_as_written() {
        let ratings = parsed(&with_ratings("update_interval_seconds = 3600\n")).ratings;

        assert_eq!(ratings.update_interval(), Duration::from_secs(3_600));
    }

    #[test]
    fn an_update_cadence_of_zero_is_refused_where_it_is_written() {
        // An interval of zero is an update that is always due.
        let message = rejected(&with_ratings("update_interval_seconds = 0\n"));

        assert!(message.contains("update_interval_seconds"), "{message}");
    }

    #[test]
    fn an_update_cadence_that_does_not_divide_a_day_is_refused_naming_the_key() {
        // The updates run on a grid measured from UTC midnight, so an interval
        // that does not divide a day would put the last update of one day at a
        // different offset from the first of the next.
        for written in [7, 250, 1_000, 3_601, 90_000] {
            let message = rejected(&with_ratings(&format!(
                "update_interval_seconds = {written}\n"
            )));

            assert!(
                message.contains("update_interval_seconds"),
                "{written}: {message}"
            );
            assert!(message.contains("86400"), "{written}: {message}");
        }
    }

    #[test]
    fn every_divisor_of_a_day_is_an_update_cadence_this_server_runs_on() {
        // The ends included: one update a second, and one a day.
        for written in [1, 60, 300, 900, 3_600, 43_200, 86_400] {
            let ratings = parsed(&with_ratings(&format!(
                "update_interval_seconds = {written}\n"
            )))
            .ratings;

            assert_eq!(ratings.update_interval(), Duration::from_secs(written));
        }
    }

    #[test]
    fn a_cadence_key_named_for_publication_is_refused_by_name() {
        // The key is `update_interval_seconds`, and there is no alias for it.
        let message = rejected(&with_ratings("publication_interval_seconds = 900\n"));

        assert!(message.contains("unknown"), "{message}");
        assert!(
            message.contains("publication_interval_seconds"),
            "{message}"
        );
    }

    #[test]
    fn the_algorithms_own_numbers_are_not_configuration() {
        // The five values of the rating fit are constants in
        // `services::rating`, so the keys do not exist.
        for key in [
            "half_life_days",
            "flat_days",
            "cutoff_days",
            "minimum_games",
            "disconnect_plies",
        ] {
            let message = rejected(&with_ratings(&format!("{key} = 1\n")));
            assert!(message.contains("unknown"), "{key}: {message}");
            assert!(message.contains(key), "{key}: {message}");
        }
    }

    #[test]
    fn a_unit_multiplies_the_largest_sendable_count_without_overflowing() {
        assert_eq!(
            TimeUnit::Minute.duration(u32::MAX),
            Duration::from_secs(u64::from(u32::MAX) * 60)
        );
        assert_eq!(
            TimeUnit::Millisecond.duration(1_500),
            Duration::from_secs(1) + Duration::from_millis(500)
        );
    }
}
