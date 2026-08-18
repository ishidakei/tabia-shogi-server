//! The server's own TOML configuration, as deserialized.
//!
//! The operator's inputs split in two: the server's **configuration** —
//! authentication mode, time settings, paths — is TOML and lives here, while a
//! position collection is **data** and lives in the plain-text file this
//! configuration points at (`storage/collections.rs`). Nothing in this module
//! reads that file; it holds the path and no more.
//!
//! **The shape is the designed shape.** [`TimeConfig`] carries the time
//! settings field for field, `Duration`s included, so that the clock reads the
//! designed type rather than a configuration-flavored relative of it.
//!
//! **Numbers are counts of `time_unit`.** An operator writes the numbers that
//! will appear on the wire, and the multiplication into a [`Duration`] happens
//! once, here. The configured numbers and the `Time` block's numbers (P-2)
//! therefore coincide by construction rather than by conversion.
//!
//! **Unknown anything is an error.** An unknown key at any level, an unknown
//! `auth_mode`, an unknown `time_unit`, an unknown `side`: each fails parsing
//! naming the offender. O-1 promises that an invalid configuration fails at
//! startup, and a typo that is silently ignored is that promise broken in the
//! one way an operator cannot see — the setting simply never takes effect.

use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::game::Color;

use super::timestamp::FirstRound;

/// The whole of what the configuration file says.
///
/// Nothing here is derived or defaulted beyond what the TOML states: a value
/// this struct holds is a value an operator wrote, so a question about the
/// server's behavior is answered by reading their file.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Which authentication mode the instance runs (E2).
    pub auth_mode: AuthMode,

    /// The position collection to play from.
    ///
    /// Whether the file is there, is readable, and holds usable entries is
    /// [`Collection::load`]'s answer, at the point where it is opened. Asking
    /// here would be a second opinion able to disagree with the first.
    ///
    /// [`Collection::load`]: crate::storage::Collection::load
    pub positions: PathBuf,

    /// The `Max_Moves` limit and the playable remainder it implies. Absent
    /// means no limit, and with no limit the margin rule has nothing to say.
    pub limit: Option<Limit>,

    /// The time settings every game is played under.
    pub time: TimeConfig,

    /// How the process itself runs: where it listens, and the two limits the
    /// runtime enforces per connection and per pairing.
    pub server: ServerConfig,

    /// When matchmaking rounds run.
    ///
    /// The whole table defaults, because every key in it does: an operator who
    /// has said nothing about the schedule gets the hourly-ish rhythm B1 asks
    /// for rather than a startup failure about a table they have never heard
    /// of.
    #[serde(default)]
    pub matchmaking: MatchmakingConfig,
}

impl Config {
    /// Parses a configuration's TOML text.
    ///
    /// Text rather than a path, so that the parser is testable with no
    /// filesystem — [`Collection::parse`]'s split, for the same reason. Reading
    /// the file is the startup wiring's.
    ///
    /// The error is `toml`'s own, unwrapped: it already names the offending key
    /// and where in the file it sits, which is exactly what O-1 asks a startup
    /// failure to say. A wrapper here could only restate that less precisely,
    /// and would throw away the span.
    ///
    /// [`Collection::parse`]: crate::storage::Collection::parse
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// The `agreement_timeout_seconds` default: P-3's 120 seconds.
///
/// A function because that is the shape `#[serde(default = "...")]` takes. The
/// number itself is [`AGREEMENT_TIMEOUT`]'s, which `config` cannot name — the
/// edge would point at `session` — so the two are one value written twice and
/// the doc comment on each names the other.
///
/// [`AGREEMENT_TIMEOUT`]: crate::session::AGREEMENT_TIMEOUT
const fn default_agreement_timeout_seconds() -> u64 {
    120
}

/// How the process runs: the listener address, and the two limits the runtime
/// enforces.
///
/// **These are operational settings, not game time**, so `time_unit` does not
/// apply to any of them and the one duration key carries its unit in its name.
/// Mixing them into `[time]` would make an operator's `total = 600` and a
/// timeout count the same kind of number, which they are not.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `host:port` for the CSA listener.
    ///
    /// **Required, with no default.** No document fixes a port, and this
    /// project does not invent values — an operator who has not said where the
    /// server listens has not configured a server. Whether the string resolves
    /// and whether the address is bindable are answered where it is bound; a
    /// second opinion here could only disagree with the first.
    ///
    /// `127.0.0.1:0` binds an ephemeral port, which is what the integration
    /// tests use: the bound address is read back from the handle
    /// [`run`](crate::run) returns.
    pub listen: String,

    /// How many malformed lines close a connection.
    ///
    /// Repeated occurrences close the connection, and no number is fixed
    /// anywhere for how many that is, and
    /// [`Disposition::Malformed`](crate::session::Disposition::Malformed)
    /// deliberately reports a count rather than deciding — so the operator
    /// supplies the limit.
    ///
    /// [`NonZeroU32`] rather than a `u32` with a check: a limit of zero would
    /// close every connection before its first line, and serde refuses it by
    /// name where it is written rather than at some later validation pass.
    pub max_malformed_lines: NonZeroU32,

    /// How long an offered pairing waits for both agreements, in seconds.
    ///
    /// P-3: "The timeout defaults to 120 seconds, matching shogi-server." The
    /// value is fed to [`agreement::expired`] as its `limit`.
    ///
    /// [`agreement::expired`]: crate::session::agreement::expired
    #[serde(default = "default_agreement_timeout_seconds")]
    pub agreement_timeout_seconds: u64,

    /// The certificate and key the CSA listener serves TLS with.
    ///
    /// **Its presence is the switch**: written, the listener is TLS; absent,
    /// the listener is plaintext TCP. P-8 makes the transport configurable
    /// "independently per deployment", and the public instance is the
    /// deployment that writes this table.
    ///
    /// A table rather than a boolean beside two path keys, because the two
    /// paths travel together and absence is already the off state — one
    /// spelling instead of three keys that have to agree. Whether the files
    /// exist and form a usable pair is [`Transport::new`]'s answer, at the point
    /// where they are read; asking here would be a second opinion able to
    /// disagree with the first.
    ///
    /// [`Transport::new`]: crate::session::Transport::new
    pub tls: Option<TlsConfig>,
}

/// Where the CSA listener's TLS material lives.
///
/// Both keys are required, because neither is usable alone: a certificate with
/// no key cannot complete a handshake, and a key with no certificate has nothing
/// to present. So the operator states both, or writes no `[server.tls]` at all —
/// [`Limit`]'s rule, for the same reason.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// The PEM certificate chain to present, leaf first.
    pub cert: PathBuf,

    /// The PEM private key belonging to that chain's leaf.
    pub key: PathBuf,
}

impl ServerConfig {
    /// [`agreement_timeout_seconds`](Self::agreement_timeout_seconds) as a
    /// duration.
    ///
    /// The conversion is here rather than in a hand-written [`Deserialize`],
    /// unlike [`TimeConfig`]'s, because this number's unit is fixed by its own
    /// key name and does not depend on a sibling: there is nothing to resolve
    /// before it can be converted, so the raw value an operator wrote stays
    /// readable on the struct.
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
/// **A round is a time, not an event.** Nothing a client does starts one: a
/// login, a discarded pairing, and a game ending each put a session in the pool,
/// and the pool waits for the next scheduled round. What a round *computes* is
/// [`matchmaker`](crate::session::matchmaker)'s and does not depend on any value
/// here.
///
/// The two rules these three keys express:
///
/// 1. The first round after startup runs at
///    [`first_round_at`](Self::first_round_at), or — unset, or already past —
///    at `startup + idle_delay_seconds`.
/// 2. After every round, the next round is at `this round + interval_seconds`;
///    and whenever the server goes from at least one game in progress to none,
///    the next round is brought forward to `that moment + idle_delay_seconds`
///    if that is earlier (the idle half amended 2026-08-18). The transition is
///    what counts, not what the most recent round did.
///
/// The `_seconds` suffix follows
/// [`agreement_timeout_seconds`](ServerConfig::agreement_timeout_seconds), for
/// the same reason: these are operational settings and `time_unit` does not
/// apply to them, so each duration carries its unit in its name.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchmakingConfig {
    /// How long after a quiet moment — startup, or the last game on the server
    /// ending — the next round runs.
    ///
    /// Zero is a setting, not a mistake: it is what the M2 gate and the
    /// integration tests run under, where a round should follow the pool
    /// immediately.
    #[serde(default = "default_idle_delay_seconds")]
    pub idle_delay_seconds: u64,

    /// The longest gap between two consecutive rounds.
    ///
    /// [`NonZeroU64`] rather than a `u64` with a check, on
    /// [`max_malformed_lines`](ServerConfig::max_malformed_lines)'s terms: an
    /// interval of zero is a round that is always due, and serde refuses it by
    /// name where it is written rather than at some later pass.
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: NonZeroU64,

    /// The wall-clock time of the **first** round after startup.
    ///
    /// Absent, or written as the empty string, means "not set": an operator
    /// commenting a scheduled launch out by emptying its value has said the
    /// same thing as one who deleted the line. From the second round on, rule 2
    /// applies and this value is never read again — including across a restart,
    /// where a value now in the past falls back to `startup +
    /// idle_delay_seconds`.
    #[serde(default, deserialize_with = "first_round_at")]
    pub first_round_at: Option<FirstRound>,
}

impl Default for MatchmakingConfig {
    /// The defaults each key documents, so that `[matchmaking]` written with
    /// one key set and `[matchmaking]` omitted altogether agree on the rest.
    fn default() -> Self {
        Self {
            idle_delay_seconds: default_idle_delay_seconds(),
            interval_seconds: default_interval_seconds(),
            first_round_at: None,
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

/// Reads `first_round_at`, where the empty string is the absent value.
///
/// A hand-written step rather than a `String` field parsed later: O-1's promise
/// is that an invalid configuration fails at startup, and a timestamp checked
/// anywhere but here is a timestamp some other caller can forget to check.
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
///
/// Exactly two settings, and no third: the enum is the requirement. What each
/// one *does* at `LOGIN` is P-1's; this slice owns only the value.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Only issued, unrevoked tokens are accepted. The public instance's mode.
    Github,

    /// Any token string is accepted and no GitHub sign-in is required to play.
    Open,
}

/// The absolute ply limit, and how much of it a game must have left to be worth
/// playing.
///
/// The two keys are one table because neither is usable alone. A
/// `min_playable_plies` with no `max_moves` constrains nothing, and a
/// `max_moves` with no minimum would need this code to invent a default
/// remainder, which this server refuses to do: the minimum playable remainder
/// is a configured value. So the operator states both, or states neither.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    /// `Max_Moves`, the absolute ply limit — **setup moves included**
    /// (invariant 5).
    pub max_moves: u32,

    /// How many plies of real play an entry must leave under `max_moves`.
    pub min_playable_plies: u32,
}

/// The unit every configured time value is counted in.
///
/// A config-local mirror of [`csa::TimeUnit`], with the same three variants and
/// the same spellings. `csa` may not depend on `config`, and `config` is given
/// no edge to `csa`, so
/// the two meet in the session layer — the reasoning [`csa::TimeSettings`]
/// records for the `Time` block as a whole.
///
/// The spellings are the wire spellings, so what an operator writes is what
/// `Time_Unit:` carries.
///
/// [`csa::TimeUnit`]: crate::csa::TimeUnit
/// [`csa::TimeSettings`]: crate::csa::TimeSettings
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
    /// `count` of this unit, as a duration.
    ///
    /// The one place a configured number becomes a time. Widening to `u64`
    /// before the multiplication is what makes it total: the largest count is
    /// `u32::MAX` and the largest multiplier 60, and that product fits.
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
/// The designed time settings, field for field. The one departure is
/// the TOML key for [`unit`](Self::unit), which is written `time_unit` so that
/// it reads as a unit rather than as one more `[time]` quantity.
///
/// [`Deserialize`] is hand-written because the numbers cannot be converted
/// field by field: how many seconds `total = 600` means depends on a *sibling*
/// key. So the raw shape is read first, and the multiplication happens once
/// `time_unit` is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeConfig {
    /// `Time_Unit`, the unit the operator's numbers count.
    pub unit: TimeUnit,

    /// `Total_Time`, the initial allowance. Required: a server with no stated
    /// allowance has nothing to put in the `Time` block.
    pub total: Duration,

    /// `Byoyomi`. Absent means no byoyomi.
    pub byoyomi: Option<Duration>,

    /// `Increment`, added before each turn begins. Absent means no increment.
    ///
    /// This is also what a setup move's T-value cancels against (invariant 4):
    /// the written T equals the deduction, so under Fischer increment the two
    /// annul and a client's clock matches the server's at the first real move.
    pub increment: Option<Duration>,

    /// `Least_Time_Per_Move`, the floor on a move's charged consumption. Not
    /// optional, because the wire line is always emitted: a zero floor is
    /// written `0` rather than left to a default a reader has to know.
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
/// landing whole on the reduced side's first setup move (invariant 4, P-5). A
/// second channel for the same fact is a channel that can contradict the first,
/// which is why [`csa::TimeSettings`] has no field for it.
///
/// [`csa::TimeSettings`]: crate::csa::TimeSettings
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reduction {
    /// Whose allowance is reduced.
    pub side: Color,

    /// By how much.
    pub amount: Duration,
}

impl<'de> Deserialize<'de> for TimeConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawTime::deserialize(deserializer)?;
        let unit = raw.time_unit;

        Ok(Self {
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
        })
    }
}

/// The `[time]` table exactly as written: counts, not durations.
///
/// `u32` because that is what the wire carries — [`csa::TimeSettings`] is `u32`
/// throughout, a time being a count of units that cannot run backwards. A
/// number too large to be sent is therefore refused where it is written rather
/// than saturating somewhere downstream.
///
/// [`csa::TimeSettings`]: crate::csa::TimeSettings
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTime {
    time_unit: TimeUnit,
    total: u32,
    byoyomi: Option<u32>,
    increment: Option<u32>,
    least_time_per_move: u32,
    roundup: bool,
    reduction: Option<RawReduction>,
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
/// [`Color`] cannot carry this itself: `game/` names nothing outside `std`
/// (invariant 1), so it cannot derive [`Deserialize`]. Mirroring the two
/// variants here costs one conversion and keeps serde's own "unknown variant"
/// message, which lists the accepted spellings.
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

    /// The layout a position collection is configured through, with every key
    /// set — the asymmetric worked example, verbatim.
    const FULL: &str = "\
auth_mode = \"open\"
positions = \"assets/positions/even.txt\"

[limit]
max_moves = 512
min_playable_plies = 40

[server]
listen = \"0.0.0.0:4081\"
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
";

    /// The smallest configuration that parses: no `[limit]`, and only the
    /// required keys of `[server]` and `[time]`.
    const MINIMAL: &str = "\
auth_mode = \"github\"
positions = \"positions.txt\"

[server]
listen = \"127.0.0.1:4081\"
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = 600
least_time_per_move = 0
roundup = true
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

    /// `MINIMAL` with one line replaced, so that a test changes exactly the key
    /// it is about.
    fn with_line(replaced: &str, replacement: &str) -> String {
        assert!(MINIMAL.contains(replaced), "{replaced} is not in MINIMAL");
        MINIMAL.replace(replaced, replacement)
    }

    #[test]
    fn the_documented_layout_parses_field_by_field() {
        let config = parsed(FULL);

        assert_eq!(config.auth_mode, AuthMode::Open);
        assert_eq!(config.positions, PathBuf::from("assets/positions/even.txt"));
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
        assert_eq!(config.server.listen, "0.0.0.0:4081");
        assert_eq!(config.server.max_malformed_lines.get(), 8);
        assert_eq!(config.server.agreement_timeout(), Duration::from_secs(30));
    }

    #[test]
    fn an_unstated_agreement_timeout_is_the_120_seconds_p_3_names() {
        assert_eq!(
            parsed(MINIMAL).server.agreement_timeout(),
            crate::session::AGREEMENT_TIMEOUT
        );
    }

    #[test]
    fn a_required_server_key_may_not_be_omitted() {
        for key in ["listen", "max_malformed_lines"] {
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
    fn a_malformed_line_limit_of_zero_is_refused_where_it_is_written() {
        // A limit of zero would close every connection before its first line,
        // so it is not a setting an operator can reach by typing a number.
        let message = rejected(&with_line(
            "max_malformed_lines = 4",
            "max_malformed_lines = 0",
        ));

        assert!(message.contains('0'), "{message}");
    }

    #[test]
    fn an_unknown_server_key_is_rejected_by_name() {
        let message = rejected(&with_line(
            "max_malformed_lines = 4",
            "max_malformed_lines = 4\nquickack = true",
        ));

        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("quickack"), "{message}");
    }

    #[test]
    fn no_tls_table_is_the_plaintext_listener_p_8_leaves_unchanged() {
        assert_eq!(parsed(MINIMAL).server.tls, None);
    }

    #[test]
    fn a_tls_table_carries_the_two_pem_paths() {
        let text = format!(
            "{MINIMAL}\n[server.tls]\ncert = \"/etc/tabia/cert.pem\"\nkey = \"/etc/tabia/key.pem\"\n"
        );

        assert_eq!(
            parsed(&text).server.tls,
            Some(TlsConfig {
                cert: PathBuf::from("/etc/tabia/cert.pem"),
                key: PathBuf::from("/etc/tabia/key.pem"),
            })
        );
    }

    #[test]
    fn a_half_written_tls_table_names_the_key_that_is_missing() {
        for (table, missing) in [
            ("[server.tls]\ncert = \"cert.pem\"\n", "key"),
            ("[server.tls]\nkey = \"key.pem\"\n", "cert"),
        ] {
            let message = rejected(&format!("{MINIMAL}\n{table}"));
            assert!(message.contains(missing), "{missing}: {message}");
        }
    }

    #[test]
    fn an_unknown_tls_key_is_rejected_by_name() {
        let text = format!(
            "{MINIMAL}\n[server.tls]\ncert = \"cert.pem\"\nkey = \"key.pem\"\nciphers = \"any\"\n"
        );

        let message = rejected(&text);

        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("ciphers"), "{message}");
    }

    #[test]
    fn a_bare_tls_boolean_is_still_refused() {
        // The switch is the table's presence, so `tls = true` is not a way to
        // turn TLS on with no material behind it. It parsed as an unknown key
        // before this slice and is a rejected value now; either way an operator
        // who writes it is told, naming the key.
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
        // The operator writes the numbers that go on the wire; the unit is what
        // says how long they are.
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

[server]
listen = \"127.0.0.1:4081\"
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
    fn an_omitted_optional_time_key_is_absent_rather_than_zero() {
        let time = parsed(MINIMAL).time;

        assert_eq!(time.byoyomi, None);
        assert_eq!(time.increment, None);
        assert_eq!(time.reduction, None);
        // A zero floor is written, not defaulted, so this one is present.
        assert_eq!(time.least_time_per_move, Duration::ZERO);
    }

    #[test]
    fn a_required_time_key_may_not_be_omitted() {
        for key in ["time_unit", "total", "least_time_per_move", "roundup"] {
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
        // u32 is what the wire carries, so both ends are refused here rather
        // than saturating somewhere a client would see.
        for count in ["4294967296", "-1"] {
            let message = rejected(&with_line("total = 600", &format!("total = {count}")));
            assert!(message.contains(count), "{count}: {message}");
        }
    }

    /// `MINIMAL` with a `[matchmaking]` table appended. It goes last, which is
    /// legal TOML and keeps the appended keys out of `[time]`.
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
        // delay is not a delay; neither is reachable by typing a number.
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
