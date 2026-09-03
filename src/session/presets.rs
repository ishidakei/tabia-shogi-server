//! The preset engines: which ones a round wants running, and the processes.
//!
//! There is no manual start or stop: a preset engine the server runs is a child
//! process of it, started and stopped by how many external engines — engines
//! that are not this server's own — are waiting at each matchmaking round.
//!
//! Three kinds of registration, and [`Kind`] is the whole of the difference. A
//! `protocol = "csa"` entry is a preset the server does not run at all: the
//! operator runs its client wherever they like and it logs in over the ordinary
//! listener with the entry's token. A `protocol = "usi"` entry is a plain USI
//! engine the server runs and plays through [`bridge`](super::bridge), and its
//! `lifecycle` says whether the process is resident or started per round. Only
//! the on-demand entries are a round's to start or stop.
//!
//! [`plan`] is pure — no clock, no process, no randomness — and [`Presets`] runs
//! the processes it asks for.
//!
//! A preset-vs-external game is never broken off. Only a preset-vs-preset game
//! is, and only to make room for an engine that is not the server's own.
//!
//! A preset's designated rating is decided by configuration alone — the `rating`
//! on its entry — and read by [`Standing::estimate`] alone.
//!
//! Starting a preset is starting a process that has to connect, log in and land
//! in the waiting pool, none of which happens inside the round that decided to
//! start it: a round starts what a later round pairs. A preset the server has
//! started but has not heard from is [`State::Connecting`], and it occupies its
//! slot while it gets there.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::config::{Lifecycle, PresetEngines};

use super::bridge;
use super::transport::Transport;

/// How many preset engines may be engaged in a game at once.
///
/// A fixed value rather than a configuration key. Nothing here limits a child's
/// CPU or memory, so this count is the whole of what the server itself bounds.
///
/// It bounds presets in games, not processes, by [`Standing::occupies_a_slot`]'s
/// two accountings:
///
/// - An on-demand preset occupies one slot from the moment its process starts
///   until the process has been stopped, which for that lifecycle is the same
///   thing as counting games.
/// - An externally run or resident preset occupies one slot from being paired
///   into a game until that game ends, so waiting costs it nothing.
///
/// The bound is enforced at the two ends that can raise the count: [`plan`],
/// which starts no process that would exceed it, and the pairing, which makes no
/// pairing that would.
pub const MAX_PLAYING: usize = 2;

/// What one registered preset is doing, as a round sees it.
///
/// What each state costs is not the state's alone: see
/// [`Standing::occupies_a_slot`], which reads it together with who runs the
/// preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No process, and no session. It occupies no slot, whoever runs it.
    Stopped,

    /// The server has started the process and has not seen it log in yet. It
    /// occupies a slot, because it is on its way to the pool.
    Connecting,

    /// Logged in and waiting in the pool. The round running now can pair it.
    Idle,

    /// Playing an engine that is not one of this server's presets. Never broken
    /// off.
    PlayingExternal,

    /// Playing another preset. The one game a round may break off, and only to
    /// free a slot for an external engine.
    PlayingPreset,
}

impl State {
    /// Whether this preset is in a game — the states no stop may touch.
    const fn is_playing(self) -> bool {
        matches!(self, Self::PlayingExternal | Self::PlayingPreset)
    }
}

/// Which of the three shapes a registered preset has.
///
/// Configuration alone — the entry's `protocol` and, for a USI entry, its
/// `lifecycle` — so it is the same answer every round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `protocol = "csa"`: a preset the server does not run. No process to start
    /// or stop, and a slot occupied only while it is in a game.
    ExternallyRun,

    /// `protocol = "usi"`, `lifecycle = "resident"`: a process the server starts
    /// when it starts and leaves up. No round starts or stops it, and it
    /// occupies a slot only while it is in a game.
    Resident,

    /// `protocol = "usi"`, `lifecycle = "on-demand"`: a preset that exists only
    /// for the rounds that want it. A round starts it and stops it, and it
    /// occupies a slot from the moment its process starts until that process has
    /// been stopped.
    OnDemand,
}

impl Kind {
    /// Whether pairing a preset of this kind into a game is what takes its slot.
    ///
    /// The other side of [`Standing::occupies_a_slot`], for the pairing rather
    /// than for the plan: an on-demand preset's slot has been occupied since its
    /// process started, so the game it was started for is exactly the game the
    /// cap must not withhold.
    pub const fn pays_on_pairing(self) -> bool {
        !matches!(self, Self::OnDemand)
    }
}

/// One registered preset, as [`plan`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    /// What it is doing.
    pub state: State,

    /// Which of the three shapes it has.
    pub kind: Kind,

    /// What it is estimated to be worth, on the rates the pairing search reads:
    /// its designated rating if it has one, otherwise the published figure, and
    /// otherwise the matchmaker's default.
    ///
    /// Read only for choosing which preset to start for an odd round. Which
    /// preset actually meets the leftover external engine is the pairing's.
    pub estimate: i32,
}

impl Standing {
    /// Whether this preset occupies one of the [`MAX_PLAYING`] slots right now.
    ///
    /// An on-demand preset pays from the moment its process starts, so every
    /// state but [`Stopped`](State::Stopped) occupies a slot. The other two
    /// kinds pay only while they are in a game.
    pub const fn occupies_a_slot(&self) -> bool {
        match self.kind {
            Kind::ExternallyRun | Kind::Resident => self.state.is_playing(),
            Kind::OnDemand => !matches!(self.state, State::Stopped),
        }
    }

    /// Whether a round could start a process for this preset.
    ///
    /// Only an on-demand preset is a round's to start: an externally run one has
    /// no command, and a resident one is started and restarted by the
    /// supervisor, so a round that started one would be racing it. And a preset
    /// that is not [`Stopped`](State::Stopped) is one this server would be
    /// starting a second instance of.
    const fn is_startable(&self) -> bool {
        matches!(self.kind, Kind::OnDemand) && matches!(self.state, State::Stopped)
    }

    /// Whether this preset could be brought into a game by a round, ignoring
    /// the cap.
    ///
    /// Everything but a preset no round can start and that is not there: one the
    /// server does not run and nobody has logged in, and a resident one whose
    /// process is down.
    const fn is_available(&self) -> bool {
        !self.state.is_playing() && (self.is_startable() || !matches!(self.state, State::Stopped))
    }
}

/// What a round does about the preset engines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Presets to start, as indices into the configured list. Each is
    /// [`State::Stopped`] now and wanted by this round.
    pub start: Vec<usize>,

    /// Presets to stop: running, and not wanted by this round. Never one that
    /// is in a game — a game is ended by [`abort`](Self::abort) or not at all.
    pub stop: Vec<usize>,

    /// Whether the preset-vs-preset game in progress is to be broken off, to
    /// free slots for an external engine that would otherwise sit out.
    pub abort: bool,
}

impl Plan {
    /// Whether this plan changes nothing.
    pub fn is_empty(&self) -> bool {
        self.start.is_empty() && self.stop.is_empty() && !self.abort
    }
}

/// What one round does about the preset engines.
///
/// `externals` holds the estimated rate of each waiting **external** engine, so
/// its length is the count the rules turn on and its contents are what the
/// odd-round choice aims at. `presets` describes every registered preset, in
/// the configured order, which is the order every index in the returned [`Plan`]
/// is read against.
///
/// The module documentation states the rules; what follows them here is the
/// bookkeeping:
///
/// - A preset that is playing is always wanted, so it is never stopped. Ending a
///   game is [`Plan::abort`]'s.
/// - A preset that is running and not wanted is stopped, including one started
///   for a round whose pool has since changed shape.
/// - Only an on-demand preset appears in either list; the other two kinds are
///   still wanted or not like any other preset, but wanting one costs no action.
/// - Ties are broken by configuration order, and a preset already running beats
///   one that would have to be started. Nothing here draws a random number.
pub fn plan(externals: &[i32], presets: &[Standing]) -> Plan {
    let wanted = wanted(externals, presets);

    Plan {
        start: (0..presets.len())
            .filter(|index| wanted.contains(index) && presets[*index].is_startable())
            .collect(),
        stop: (0..presets.len())
            .filter(|index| {
                let preset = &presets[*index];
                !wanted.contains(index)
                    && matches!(preset.kind, Kind::OnDemand)
                    && preset.occupies_a_slot()
                    && !preset.state.is_playing()
            })
            .collect(),
        abort: aborts(externals, presets),
    }
}

/// Which presets this round wants running, as indices into `presets`.
///
/// Every preset in a game is in it, always, which is what makes a
/// preset-vs-external game never aborted.
fn wanted(externals: &[i32], presets: &[Standing]) -> Vec<usize> {
    let mut wanted: Vec<usize> = (0..presets.len())
        .filter(|index| presets[*index].state.is_playing())
        .collect();
    // The slots a round cannot free: a preset in a game occupies one whoever
    // runs it, and no plan ends a game except the one abort below.
    let free = MAX_PLAYING.saturating_sub(wanted.len());

    if externals.is_empty() {
        // A calibration game is already being played, so the rule that starts
        // one has nothing to do.
        let calibrating = presets
            .iter()
            .any(|preset| preset.state == State::PlayingPreset);
        if !calibrating && free >= 2 {
            wanted.extend(calibration_pair(presets));
        }

        return wanted;
    }

    // An even pool of external engines has itself, and a preset joining it
    // would only make it odd again.
    if externals.len().is_multiple_of(2) {
        return wanted;
    }

    if let Some(joining) = joins_the_leftover(externals, presets, free) {
        wanted.push(joining);
    }

    wanted
}

/// The two presets a calibration round makes ready, or nothing.
///
/// Designation constrains nothing here, so the two are simply the first two
/// available — not playing, and either already there or startable — preferring
/// the ones already there, since a preset already waiting can be paired by the
/// very next round. A preset with no session that no round can start is the one
/// entry that is not available at all.
fn calibration_pair(presets: &[Standing]) -> Vec<usize> {
    let mut available: Vec<usize> = (0..presets.len())
        .filter(|index| presets[*index].is_available())
        .collect();
    // Already logged in or already running first, then configuration order.
    // `sort_by_key` is stable, so the second half of that is the order the
    // indices already have.
    available.sort_by_key(|index| usize::from(presets[*index].state == State::Stopped));
    available.truncate(2);

    if available.len() < 2 {
        return Vec::new();
    }

    available
}

/// Which preset an odd round wants for the external engine that would otherwise
/// be left over, or `None`.
///
/// Three answers in order, and the order is the rule: a preset already waiting
/// costs nothing and can be paired now; a preset already on its way is the one a
/// previous round started for this, and starting a second would waste the other
/// slot; otherwise one is started, if a slot is free.
///
/// The one that is started is the one whose estimate is closest to the mean of
/// the waiting external engines. The binding closest-rated rule is the pairing's;
/// this is a start-time guess made before the round knows which external engine
/// will be the leftover.
///
/// A waiting preset of any kind is one of the first two answers. Only the third
/// is restricted, to the on-demand entries a round may start.
fn joins_the_leftover(externals: &[i32], presets: &[Standing], free: usize) -> Option<usize> {
    if let Some(waiting) = closest(externals, presets, |preset| preset.state == State::Idle) {
        return Some(waiting);
    }

    if let Some(coming) =
        (0..presets.len()).find(|index| presets[*index].state == State::Connecting)
    {
        return Some(coming);
    }

    if free == 0 {
        return None;
    }

    closest(externals, presets, Standing::is_startable)
}

/// The preset `admits` accepts whose estimate is closest to the external
/// engines' mean, with a tie going to the earlier configuration entry.
fn closest(
    externals: &[i32],
    presets: &[Standing],
    admits: impl Fn(&Standing) -> bool,
) -> Option<usize> {
    let target = mean(externals)?;

    (0..presets.len())
        .filter(|index| admits(&presets[*index]))
        .min_by_key(|index| presets[*index].estimate.abs_diff(target))
}

/// The mean estimated rate of the waiting external engines, or `None` for none.
///
/// Summed as an `i64` so that a pool of engines at the top of the range cannot
/// overflow the accumulator, and rounded toward zero, which is enough for a
/// comparison of distances.
fn mean(externals: &[i32]) -> Option<i32> {
    if externals.is_empty() {
        return None;
    }

    let total: i64 = externals.iter().map(|rate| i64::from(*rate)).sum();
    let mean = total / externals.len() as i64;

    i32::try_from(mean).ok()
}

/// Whether the preset-vs-preset game in progress is to be broken off.
///
/// Exactly the case the rules name: an odd number of external engines is
/// waiting, no preset can join it without a slot being freed, every slot is
/// occupied, and one of the games occupying them is between two presets.
/// Breaking it off frees both slots, and the round after this one starts a
/// preset for the engine that sat out.
///
/// "No preset can join it" is read as a preset whose slot is already paid for:
/// one this server is running that is not in a game. A waiting externally run
/// preset is not one of those — its slot is taken when it is paired, and with
/// every slot occupied there is none to take.
fn aborts(externals: &[i32], presets: &[Standing]) -> bool {
    if externals.is_empty() || externals.len().is_multiple_of(2) {
        return false;
    }

    let ready = presets
        .iter()
        .any(|preset| preset.occupies_a_slot() && !preset.state.is_playing());
    let held = presets
        .iter()
        .filter(|preset| preset.occupies_a_slot())
        .count();

    !ready
        && held >= MAX_PLAYING
        && presets
            .iter()
            .any(|preset| preset.state == State::PlayingPreset)
}

/// The first wait before a resident engine that exited is started again.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);

/// The longest that wait ever grows to.
///
/// A minute: a bounded cost for a bridge that fails on every attempt, and still
/// short enough that an engine an operator has just fixed is back inside one.
const LONGEST_BACKOFF: Duration = Duration::from_secs(60);

/// How long a resident engine's bridge must have run for its exit to count as a
/// fresh failure rather than another of the same one.
///
/// An engine that came up, logged in, played for a while and then died is not in
/// a restart loop.
const STEADY: Duration = LONGEST_BACKOFF;

/// The wait after `attempts` consecutive failures, counted from one.
///
/// Doubling from [`FIRST_BACKOFF`] and capped at [`LONGEST_BACKOFF`]. The shift
/// is bounded before it is made, so an engine that has failed thirty times is
/// waiting a minute rather than overflowing into no wait at all.
fn backoff(attempts: u32) -> Duration {
    let steps = attempts.saturating_sub(1).min(u32::BITS - 1);

    FIRST_BACKOFF
        .saturating_mul(1u32 << steps)
        .min(LONGEST_BACKOFF)
}

/// The preset engines' processes: the half of this module that touches the
/// operating system.
///
/// One bridge per running preset, keyed by its index in the configured list, so
/// the same preset never runs more than one instance.
///
/// What it spawns is [`bridge::run`](super::bridge::run), which owns the
/// engine's process, so stopping a preset is one thing — ending the task —
/// rather than two that could get out of step.
///
/// Nothing is passed to a child: no arguments are appended to the command the
/// operator wrote, no environment is set, and the preset's token is never put on
/// the child's command line, where every process on the host could read it out
/// of `/proc`. No resource limit is applied here either.
#[derive(Debug)]
pub struct Presets {
    /// The registered presets, in configuration order.
    registered: PresetEngines,

    /// Where the bridges dial: the address the listener is actually bound on.
    address: SocketAddr,

    /// What the listener wraps a connection in, so a bridge's dial matches it.
    transport: Transport,

    /// The running bridges, by index into [`registered`](Self::registered).
    running: HashMap<usize, Bridged>,

    /// What is known about each resident preset's restarts, by the same index.
    ///
    /// Only residents have an entry: they are the only presets this supervisor
    /// starts on its own initiative.
    restarts: HashMap<usize, Restart>,
}

/// One running bridge.
#[derive(Debug)]
struct Bridged {
    /// The task. Aborting it drops the engine's child process, which carries
    /// `kill_on_drop`, so there is exactly one way for a preset's process to end.
    task: JoinHandle<()>,

    /// Where "stop" travels, so that a preset being stopped gets to send its
    /// engine a `quit` before the abort lands.
    stop: oneshot::Sender<()>,

    /// When it was started, which is what [`STEADY`] is measured against.
    started: Instant,
}

/// A resident preset's restart bookkeeping.
#[derive(Clone, Copy, Debug)]
struct Restart {
    /// How many consecutive failures it has had, counted from one.
    attempts: u32,

    /// The earliest moment the next attempt may be made.
    not_before: Instant,
}

impl Presets {
    /// A supervisor over these registered presets, with nothing running.
    ///
    /// The address and the transport are the listener's own, read after it was
    /// bound, so a bridge dials the address the server is actually on rather
    /// than the one the file asked for.
    pub fn new(registered: PresetEngines, address: SocketAddr, transport: Transport) -> Self {
        Self {
            registered,
            address,
            transport,
            running: HashMap::new(),
            restarts: HashMap::new(),
        }
    }

    /// How many presets are registered.
    pub fn len(&self) -> usize {
        self.registered.len()
    }

    /// Whether the instance registers no preset engines at all.
    pub fn is_empty(&self) -> bool {
        self.registered.is_empty()
    }

    /// The rating designated for the preset at `index`, if it has one
    /// (configuration alone).
    pub fn designated_rating(&self, index: usize) -> Option<i32> {
        self.registered.designated_rating(index)
    }

    /// Which of the three shapes the preset at `index` has (configuration
    /// alone).
    ///
    /// An index this list has no entry at answers
    /// [`ExternallyRun`](Kind::ExternallyRun), the answer that never asks for a
    /// process to be started.
    pub fn kind(&self, index: usize) -> Kind {
        match self.registered.lifecycle(index) {
            None => Kind::ExternallyRun,
            Some(Lifecycle::Resident) => Kind::Resident,
            Some(Lifecycle::OnDemand) => Kind::OnDemand,
        }
    }

    /// How many registered presets the operator runs rather than this server.
    ///
    /// For the startup line that says what has been registered. Nothing decides
    /// anything by it.
    pub fn externally_run(&self) -> usize {
        self.registered
            .iter()
            .filter(|preset| preset.is_externally_run())
            .count()
    }

    /// How many registered presets are USI engines this server runs.
    ///
    /// For the same startup line, and read by nothing else.
    pub fn bridged(&self) -> usize {
        self.registered.len() - self.externally_run()
    }

    /// Whether this server currently owns a bridge for the preset at `index`.
    pub fn is_running(&self, index: usize) -> bool {
        self.running.contains_key(&index)
    }

    /// Drops every bridge that has ended, and starts every resident engine whose
    /// wait has run out.
    ///
    /// Called before a round reads the states, and once when the coordinator
    /// starts. A preset whose bridge ended must stop holding a slot and become
    /// startable again, and a resident one must additionally be started again,
    /// since no round will do it.
    ///
    /// The clock is a parameter rather than read here, so the backoff is
    /// arithmetic a test can drive.
    pub fn maintain(&mut self, now: Instant) {
        let ended: Vec<usize> = self
            .running
            .iter()
            .filter(|(_, bridged)| bridged.task.is_finished())
            .map(|(index, _)| *index)
            .collect();

        for index in ended {
            let Some(bridged) = self.running.remove(&index) else {
                continue;
            };
            info!(
                preset = index,
                "a preset engine's bridge is no longer running"
            );

            if matches!(self.kind(index), Kind::Resident) {
                // A bridge that had been up a good while is a fresh failure, so
                // its wait starts over.
                let previous = self.restarts.get(&index).map_or(0, |restart| {
                    if now.duration_since(bridged.started) >= STEADY {
                        0
                    } else {
                        restart.attempts
                    }
                });
                let attempts = previous.saturating_add(1);
                let wait = backoff(attempts);
                info!(
                    preset = index,
                    attempts,
                    in_seconds = wait.as_secs(),
                    "a resident preset engine will be started again",
                );
                self.restarts.insert(
                    index,
                    Restart {
                        attempts,
                        not_before: now + wait,
                    },
                );
            }
        }

        for index in 0..self.registered.len() {
            if !matches!(self.kind(index), Kind::Resident) || self.is_running(index) {
                continue;
            }
            let due = self
                .restarts
                .get(&index)
                .is_none_or(|restart| now >= restart.not_before);
            if due {
                self.start(index);
            }
        }
    }

    /// Starts the preset at `index`, unless it is already running.
    ///
    /// A registration the server does not run is refused here as well as in
    /// every plan: saying so beats starting nothing in silence.
    ///
    /// Nothing about the bridge is awaited: spawning the engine, shaking hands
    /// with it and logging in take as long as the engine takes. The round learns
    /// the outcome from the state the preset is in next time.
    pub fn start(&mut self, index: usize) {
        if self.running.contains_key(&index) {
            return;
        }
        let Some(preset) = self.registered.get(index) else {
            return;
        };
        if preset.is_externally_run() {
            warn!(
                preset = index,
                "a preset engine the operator runs was asked to be started"
            );
            return;
        }
        let Some(program) = preset.command.first() else {
            // Refused at startup, so this is unreachable from a configuration
            // that loaded.
            error!(
                preset = index,
                "a USI preset engine has no command to start"
            );
            return;
        };

        let (stop, stopped) = oneshot::channel();
        let engine = bridge::Engine {
            preset: index,
            token: preset.token.clone(),
            command: preset.command.clone(),
            options: preset
                .usi_options
                .iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
            name: preset.name.clone(),
            address: self.address,
            transport: self.transport.clone(),
        };

        info!(preset = index, program, "a preset engine is being started");
        self.running.insert(
            index,
            Bridged {
                task: tokio::spawn(bridge::run(engine, stopped)),
                stop,
                started: Instant::now(),
            },
        );
    }

    /// Stops the preset at `index`, if this server started it.
    ///
    /// A preset that is not running is not an error. Nothing is stopped that
    /// this server did not start: a session logged in on a preset's token from
    /// somewhere else is the operator's process.
    ///
    /// Asked first, ended second. The bridge is told to stop, which gives it the
    /// chance to send its engine a `quit`; a watchdog aborts the task if it has
    /// not finished within [`GRACE`], and aborting drops the child, which
    /// carries `kill_on_drop`. Neither half waits here.
    pub fn stop(&mut self, index: usize) {
        let Some(Bridged { task, stop, .. }) = self.running.remove(&index) else {
            return;
        };

        info!(preset = index, "a preset engine is being stopped");
        tokio::spawn(async move {
            let _ = stop.send(());

            let mut task = task;
            if tokio::time::timeout(GRACE, &mut task).await.is_err() {
                debug!(
                    preset = index,
                    "a preset engine did not quit in time and is being ended"
                );
                task.abort();
            }
        });
    }

    /// Stops every running preset. What shutdown owes the host.
    pub fn stop_all(&mut self) {
        for index in self.running.keys().copied().collect::<Vec<_>>() {
            self.stop(index);
        }
    }
}

/// How long a stopped preset is given to quit before its task is ended.
///
/// Long enough for an engine to notice `quit` and exit, and short enough that a
/// server shutting down is not held up by one that will not.
const GRACE: Duration = Duration::from_secs(5);

impl Drop for Presets {
    /// Ends every bridge still running.
    ///
    /// A dropped [`JoinHandle`] **detaches** its task rather than ending it, so
    /// without this a coordinator that stopped would leave its engines running
    /// with nothing to play — the one thing the whole supervisor exists to
    /// prevent. Aborting drops each bridge, and with it the child process it
    /// owns.
    fn drop(&mut self) {
        for bridged in self.running.values() {
            bridged.task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An on-demand preset the server runs, in a state, at a middling estimate.
    const fn standing(state: State) -> Standing {
        Standing {
            state,
            kind: Kind::OnDemand,
            estimate: 2_000,
        }
    }

    /// A preset the operator runs, in a state, at a middling estimate.
    const fn outside(state: State) -> Standing {
        Standing {
            kind: Kind::ExternallyRun,
            ..standing(state)
        }
    }

    /// A resident preset the server runs, in a state.
    const fn resident(state: State) -> Standing {
        Standing {
            kind: Kind::Resident,
            ..standing(state)
        }
    }

    /// A preset at a stated estimate.
    const fn rated(state: State, estimate: i32) -> Standing {
        Standing {
            estimate,
            ..standing(state)
        }
    }

    #[test]
    fn no_preset_is_registered_and_nothing_is_planned() {
        for externals in [&[][..], &[2_000][..], &[2_000, 2_100][..]] {
            assert!(plan(externals, &[]).is_empty(), "{externals:?}");
        }
    }

    #[test]
    fn an_empty_pool_starts_a_calibration_pair() {
        // No external engine waiting, two presets stopped, both slots free: the
        // two are started, and the round after this one pairs them.
        let planned = plan(&[], &[standing(State::Stopped); 2]);

        assert_eq!(planned.start, vec![0, 1]);
        assert!(planned.stop.is_empty());
        assert!(!planned.abort);
    }

    #[test]
    fn a_calibration_pair_is_the_first_two_available_whatever_is_designated() {
        // A `Standing` has no field to hold a designation, so a register in
        // which every preset carries one starts a game like any other.
        let planned = plan(&[], &[standing(State::Stopped); 3]);

        assert_eq!(planned.start, vec![0, 1], "{planned:?}");
        assert!(planned.stop.is_empty(), "{planned:?}");
    }

    #[test]
    fn one_registered_preset_never_plays_itself() {
        let planned = plan(&[], &[standing(State::Stopped)]);

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_calibration_game_in_progress_starts_nothing_more() {
        // Both slots are held by the game being played, and a second
        // preset-vs-preset game is exactly what the cap forbids.
        let planned = plan(
            &[],
            &[
                standing(State::PlayingPreset),
                standing(State::PlayingPreset),
                standing(State::Stopped),
            ],
        );

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_preset_playing_an_external_engine_leaves_no_room_for_a_calibration_pair() {
        // A game from an earlier round is still running with one slot, so two
        // slots are not available and no calibration game starts.
        let planned = plan(
            &[],
            &[
                standing(State::PlayingExternal),
                standing(State::Stopped),
                standing(State::Stopped),
            ],
        );

        assert_eq!(planned.start, Vec::<usize>::new());
        assert!(!planned.abort);
    }

    #[test]
    fn an_even_pool_of_external_engines_starts_no_preset() {
        for externals in [&[2_000, 2_100][..], &[2_000, 2_100, 2_200, 2_300][..]] {
            let planned = plan(externals, &[standing(State::Stopped); 2]);

            assert!(planned.is_empty(), "{externals:?}: {planned:?}");
        }
    }

    #[test]
    fn an_even_pool_stops_a_preset_that_is_waiting_for_nothing() {
        // "No idling processes": a preset started for a round that has since
        // changed shape does not sit in the pool.
        let planned = plan(
            &[2_000, 2_100],
            &[standing(State::Idle), standing(State::Connecting)],
        );

        assert_eq!(planned.stop, vec![0, 1]);
        assert!(planned.start.is_empty());
    }

    #[test]
    fn an_odd_pool_starts_one_preset() {
        let planned = plan(&[2_000], &[standing(State::Stopped); 2]);

        assert_eq!(planned.start.len(), 1, "{planned:?}");
        assert!(!planned.abort);
    }

    #[test]
    fn the_preset_started_for_an_odd_pool_is_the_closest_to_the_pool() {
        // Three externals averaging 2100, and presets at 1200, 2090 and 3000.
        let planned = plan(
            &[2_000, 2_100, 2_200],
            &[
                rated(State::Stopped, 1_200),
                rated(State::Stopped, 2_090),
                rated(State::Stopped, 3_000),
            ],
        );

        assert_eq!(planned.start, vec![1]);
    }

    #[test]
    fn an_odd_pool_with_a_preset_already_waiting_starts_nothing() {
        // The waiting one is what the round pairs, and which engine it meets is
        // the pairing's closest-rated rule rather than this module's.
        let planned = plan(&[2_000], &[standing(State::Idle), standing(State::Stopped)]);

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn an_odd_pool_with_a_preset_on_its_way_starts_no_second_one() {
        let planned = plan(
            &[2_000],
            &[standing(State::Connecting), standing(State::Stopped)],
        );

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_surplus_waiting_preset_is_stopped_and_the_closest_is_kept() {
        // Only one preset joins an odd pool, so the other is not wanted — and
        // the one kept is the one nearest the pool.
        let planned = plan(
            &[2_000],
            &[rated(State::Idle, 2_900), rated(State::Idle, 2_010)],
        );

        assert_eq!(planned.stop, vec![0]);
        assert!(planned.start.is_empty());
    }

    #[test]
    fn an_odd_pool_with_no_slot_breaks_off_the_calibration_game() {
        // Both slots are held by a preset-vs-preset game, and an external
        // engine would sit out. The game is broken off; the next round starts a
        // preset with the slot it freed.
        let planned = plan(&[2_000], &[standing(State::PlayingPreset); 2]);

        assert!(planned.abort);
        // Nothing is started: the slots are not free until the game actually
        // ends.
        assert!(planned.start.is_empty(), "{planned:?}");
        assert!(planned.stop.is_empty(), "{planned:?}");
    }

    #[test]
    fn an_odd_pool_never_breaks_off_a_game_against_an_external_engine() {
        // Both slots held, but by games with participants in them, so one
        // external engine sits out this round.
        let planned = plan(&[2_000], &[standing(State::PlayingExternal); 2]);

        assert!(!planned.abort, "{planned:?}");
        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn an_odd_pool_with_one_slot_free_starts_rather_than_aborting() {
        // A preset-vs-preset game cannot be holding both slots here, so there
        // is nothing to abort and the free slot is used.
        let planned = plan(
            &[2_000],
            &[standing(State::PlayingExternal), standing(State::Stopped)],
        );

        assert_eq!(planned.start, vec![1]);
        assert!(!planned.abort);
    }

    #[test]
    fn an_even_pool_never_breaks_off_a_calibration_game() {
        // The rules abort only to free a slot for an external engine that would
        // otherwise sit out, and an even pool has nobody sitting out.
        let planned = plan(&[2_000, 2_100], &[standing(State::PlayingPreset); 2]);

        assert!(!planned.abort, "{planned:?}");
        assert!(planned.is_empty(), "{planned:?}");
    }

    /// Every arrangement of three presets: each of the five states, each run by
    /// in each of the three kinds, against pools of nought to three external
    /// engines.
    ///
    /// 15³ registers times four pools. The states a real server cannot be in are
    /// skipped by the caller rather than here.
    fn every_register() -> impl Iterator<Item = (Vec<Standing>, &'static [i32])> {
        const STATES: [State; 5] = [
            State::Stopped,
            State::Connecting,
            State::Idle,
            State::PlayingExternal,
            State::PlayingPreset,
        ];
        const POOLS: [&[i32]; 4] = [&[], &[2_000], &[2_000, 2_100], &[2_000, 2_100, 2_200]];
        const KINDS: [Kind; 3] = [Kind::ExternallyRun, Kind::Resident, Kind::OnDemand];
        const SHAPES: usize = KINDS.len() * STATES.len();

        (0..SHAPES.pow(3)).flat_map(|code| {
            let presets: Vec<Standing> = (0..3u32)
                .map(|position| {
                    let shape = code / SHAPES.pow(position) % SHAPES;

                    Standing {
                        state: STATES[shape / KINDS.len()],
                        kind: KINDS[shape % KINDS.len()],
                        estimate: 2_000,
                    }
                })
                .collect();

            POOLS.map(move |externals| (presets.clone(), externals))
        })
    }

    /// How many of the cap's slots this register occupies.
    fn occupied(presets: &[Standing]) -> usize {
        presets
            .iter()
            .filter(|preset| preset.occupies_a_slot())
            .count()
    }

    #[test]
    fn no_plan_ever_occupies_more_slots_than_the_cap() {
        for (presets, externals) in every_register() {
            // A register that already breaks the cap is not a state this server
            // can reach, so it is not a case to hold the plan to.
            if occupied(&presets) > MAX_PLAYING {
                continue;
            }

            let planned = plan(externals, &presets);

            let after = (0..presets.len())
                .filter(|index| {
                    let preset = &presets[*index];
                    !planned.stop.contains(index)
                        && (planned.start.contains(index) || preset.occupies_a_slot())
                })
                .count();

            assert!(
                after <= MAX_PLAYING,
                "{externals:?} over {presets:?} would occupy {after} slots: {planned:?}",
            );
        }
    }

    #[test]
    fn no_plan_starts_or_stops_a_preset_the_operator_runs() {
        // The server has no process for one.
        for (presets, externals) in every_register() {
            let planned = plan(externals, &presets);

            for index in planned.start.iter().chain(&planned.stop) {
                assert!(
                    matches!(presets[*index].kind, Kind::OnDemand),
                    "{externals:?} over {presets:?}: {planned:?}",
                );
            }
        }
    }

    #[test]
    fn no_plan_starts_a_preset_that_is_already_there() {
        for (presets, externals) in every_register() {
            let planned = plan(externals, &presets);

            for index in &planned.start {
                assert_eq!(
                    presets[*index].state,
                    State::Stopped,
                    "{externals:?} over {presets:?}: {planned:?}",
                );
            }
        }
    }

    #[test]
    fn a_preset_the_operator_runs_is_never_started_for_an_odd_pool() {
        // It is closest to the pool by a mile and still not the answer: the
        // server has no command to run for it.
        let planned = plan(
            &[2_000],
            &[
                Standing {
                    kind: Kind::ExternallyRun,
                    ..rated(State::Stopped, 2_000)
                },
                rated(State::Stopped, 3_000),
            ],
        );

        assert_eq!(planned.start, vec![1]);
    }

    #[test]
    fn a_waiting_preset_the_operator_runs_joins_an_odd_pool_like_any_other() {
        // It is already logged in, so the round wants it and starts nothing.
        let planned = plan(&[2_000], &[outside(State::Idle), standing(State::Stopped)]);

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_waiting_preset_the_operator_runs_is_never_stopped_for_idling() {
        // There is no process here, so an even pool leaves it alone.
        let planned = plan(
            &[2_000, 2_100],
            &[outside(State::Idle), outside(State::Idle)],
        );

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_waiting_resident_preset_is_never_stopped_for_idling() {
        // A resident engine waiting between games is what `lifecycle =
        // "resident"` asks for, so an even pool leaves it where it is.
        let planned = plan(
            &[2_000, 2_100],
            &[resident(State::Idle), resident(State::Idle)],
        );

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_waiting_resident_preset_joins_an_odd_pool_like_any_other() {
        let planned = plan(&[2_000], &[resident(State::Idle), standing(State::Stopped)]);

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_resident_preset_that_is_down_is_never_started_by_a_round() {
        // Starting it is the supervisor's: a round that started one as well
        // would be racing the backoff.
        let planned = plan(
            &[2_000],
            &[
                Standing {
                    kind: Kind::Resident,
                    ..rated(State::Stopped, 2_000)
                },
                rated(State::Stopped, 3_000),
            ],
        );

        assert_eq!(planned.start, vec![1]);
    }

    #[test]
    fn two_waiting_presets_the_operator_runs_are_the_calibration_pair() {
        // Both are logged in already, so the third entry — one the server could
        // have started — is not started.
        let planned = plan(
            &[],
            &[
                outside(State::Idle),
                outside(State::Idle),
                standing(State::Stopped),
            ],
        );

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_preset_the_operator_has_not_connected_is_no_calibration_partner() {
        // The first entry can neither be started nor paired, so the pair the
        // round wants is the two the server can actually put in a game.
        let planned = plan(
            &[],
            &[
                outside(State::Stopped),
                standing(State::Stopped),
                standing(State::Stopped),
            ],
        );

        assert_eq!(planned.start, vec![1, 2]);
    }

    #[test]
    fn a_game_between_presets_the_operator_runs_is_broken_off_like_any_other() {
        // Breaking off a game is an action on the game, not on a process, so it
        // applies whoever runs the two participants.
        let planned = plan(&[2_000], &[outside(State::PlayingPreset); 2]);

        assert!(planned.abort, "{planned:?}");
        assert!(planned.start.is_empty(), "{planned:?}");
        assert!(planned.stop.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_third_preset_waiting_outside_does_not_hold_back_the_abort() {
        // Both slots are occupied by the game in progress, so the waiting preset
        // cannot be paired into anything.
        let planned = plan(
            &[2_000],
            &[
                standing(State::PlayingPreset),
                standing(State::PlayingPreset),
                outside(State::Idle),
            ],
        );

        assert!(planned.abort, "{planned:?}");
    }

    #[test]
    fn a_playing_preset_is_never_stopped() {
        for state in [State::PlayingExternal, State::PlayingPreset] {
            for externals in [&[][..], &[2_000][..], &[2_000, 2_100][..]] {
                let planned = plan(externals, &[standing(state), standing(State::Stopped)]);

                assert!(
                    !planned.stop.contains(&0),
                    "{state:?} over {externals:?}: {planned:?}",
                );
            }
        }
    }

    #[test]
    fn the_wait_before_a_resident_engine_is_tried_again_doubles_and_stops() {
        assert_eq!(backoff(1), FIRST_BACKOFF);
        assert_eq!(backoff(2), FIRST_BACKOFF * 2);
        assert_eq!(backoff(3), FIRST_BACKOFF * 4);

        // An engine that has failed a hundred times is waiting a minute, not
        // overflowing into no wait at all.
        for attempts in [10, 100, u32::MAX] {
            assert_eq!(backoff(attempts), LONGEST_BACKOFF, "{attempts}");
        }
    }

    #[test]
    fn each_registration_reads_as_its_own_kind() {
        let presets = registered(
            "presets = [\
               { token = \"outside\" }, \
               { token = \"resident\", protocol = \"usi\", command = [\"/bin/sleep\"], \
                 lifecycle = \"resident\" }, \
               { token = \"on-demand\", protocol = \"usi\", command = [\"/bin/sleep\"], \
                 lifecycle = \"on-demand\" }]",
        );

        assert_eq!(presets.kind(0), Kind::ExternallyRun);
        assert_eq!(presets.kind(1), Kind::Resident);
        assert_eq!(presets.kind(2), Kind::OnDemand);
        // An index there is no entry at is the kind that asks for nothing to be
        // started.
        assert_eq!(presets.kind(3), Kind::ExternallyRun);

        assert_eq!(presets.externally_run(), 1);
        assert_eq!(presets.bridged(), 2);
    }

    /// A supervisor over the presets this TOML registers, dialling an address
    /// nothing is listening on.
    ///
    /// The dial is never reached by the tests below: each of their engines
    /// either hangs in its handshake or exits during it. A test that wants a
    /// bridge to log in needs a server — `tests/usi_presets.rs`.
    fn registered(written: &str) -> Presets {
        let table: toml::Table = toml::from_str(written).expect("the table parses");

        Presets::new(
            table
                .get("presets")
                .expect("the key is there")
                .clone()
                .try_into()
                .expect("the presets parse"),
            SocketAddr::from(([127, 0, 0, 1], 1)),
            Transport::Plain,
        )
    }

    /// A supervisor over on-demand presets that run `sleep`: a process that
    /// stays up and answers nothing, so its bridge waits out its handshake and
    /// the task stays alive.
    fn sleepers(count: usize) -> Presets {
        let written: String = (0..count)
            .map(|index| {
                format!(
                    "{{ token = \"preset-{index}\", protocol = \"usi\", \
                       lifecycle = \"on-demand\", command = [\"/bin/sleep\", \"120\"] }},"
                )
            })
            .collect();

        registered(&format!("presets = [{written}]"))
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_preset_is_started_once_and_stopped_once() {
        let mut presets = sleepers(2);
        assert_eq!(presets.len(), 2);
        assert!(!presets.is_running(0));

        presets.start(0);
        assert!(presets.is_running(0));
        assert!(!presets.is_running(1));

        // The same preset never runs more than one instance: starting it again
        // is a no-op rather than a second process.
        presets.start(0);
        assert!(presets.is_running(0));

        presets.stop(0);
        assert!(!presets.is_running(0));

        // And stopping one that is not running is the state that was asked for,
        // not an error.
        presets.stop(0);
        presets.stop(1);
        assert!(!presets.is_running(1));
    }

    /// Runs `maintain` until `condition` holds, or gives up.
    ///
    /// A bridge ends asynchronously, so what it left behind is looked for
    /// repeatedly rather than assumed to be there on the first pass.
    async fn settles(presets: &mut Presets, condition: impl Fn(&Presets) -> bool) -> bool {
        for _ in 0..200 {
            presets.maintain(Instant::now());
            if condition(presets) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        condition(presets)
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_preset_whose_engine_exited_stops_holding_its_slot() {
        // `true` exits at once, which is what an engine that dies on start-up
        // looks like from here: the bridge ends with it, and the slot comes
        // back.
        let mut presets = registered(
            "presets = [{ token = \"gone\", protocol = \"usi\", lifecycle = \"on-demand\", \
               command = [\"/bin/true\"] }]",
        );

        presets.start(0);
        assert!(presets.is_running(0));

        assert!(
            settles(&mut presets, |presets| !presets.is_running(0)).await,
            "the ended bridge still holds a slot",
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_resident_preset_is_started_by_maintain_and_started_again_when_it_dies() {
        // No plan asks for this: the supervisor starts a resident engine, and
        // when it exits the supervisor starts it again. The engine here exits at
        // once, so the second start is observed as a second start rather than as
        // the first one still running.
        let mut presets = registered(
            "presets = [{ token = \"resident\", protocol = \"usi\", lifecycle = \"resident\", \
               command = [\"/bin/true\"] }]",
        );

        presets.maintain(Instant::now());
        assert!(presets.is_running(0), "a resident engine was not started");

        // Its first attempt is one second away, so a `maintain` at that instant
        // is what starts it again.
        assert!(
            settles(&mut presets, |presets| !presets.is_running(0)).await,
            "the ended bridge was not noticed",
        );
        presets.maintain(Instant::now() + FIRST_BACKOFF);
        assert!(presets.is_running(0), "a resident engine was not restarted");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_resident_preset_waits_out_its_backoff_before_being_started_again() {
        let mut presets = registered(
            "presets = [{ token = \"resident\", protocol = \"usi\", lifecycle = \"resident\", \
               command = [\"/bin/true\"] }]",
        );

        presets.maintain(Instant::now());
        assert!(
            settles(&mut presets, |presets| !presets.is_running(0)).await,
            "the ended bridge was not noticed",
        );

        // `settles` has already called `maintain` at the present instant several
        // times, so what holds the engine down is the wait.
        assert!(!presets.is_running(0));
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_csa_preset_is_never_a_process_of_this_servers() {
        // An entry the operator runs has nothing to spawn, so a `start` leaves
        // the server owning no process for it and a `stop` has nothing to kill.
        let mut presets = registered(
            "presets = [{ token = \"run-by-the-operator\" }, \
               { token = \"run-by-the-server\", protocol = \"usi\", lifecycle = \"on-demand\", \
                 command = [\"/bin/sleep\", \"120\"] }]",
        );

        presets.start(0);
        assert!(!presets.is_running(0));
        presets.stop(0);
        assert!(!presets.is_running(0));

        // Nor is it ever started by `maintain`, which is where a resident engine
        // would have been.
        presets.maintain(Instant::now());
        assert!(!presets.is_running(0));
        assert!(!presets.is_running(1));

        // And the entry beside it is untouched by any of that.
        presets.start(1);
        assert!(presets.is_running(1));
        presets.stop_all();
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn stopping_everything_leaves_nothing_running() {
        let mut presets = sleepers(2);
        presets.start(0);
        presets.start(1);

        presets.stop_all();

        assert!(!presets.is_running(0));
        assert!(!presets.is_running(1));
    }

    #[test]
    fn a_started_and_a_stopped_preset_are_never_the_same_preset() {
        for externals in [&[][..], &[2_000][..], &[2_000, 2_100][..]] {
            for first in [State::Stopped, State::Connecting, State::Idle] {
                let planned = plan(externals, &[standing(first), standing(State::Stopped)]);

                for index in &planned.start {
                    assert!(
                        !planned.stop.contains(index),
                        "{externals:?}, {first:?}: {planned:?}",
                    );
                }
            }
        }
    }
}
