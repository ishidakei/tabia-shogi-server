//! The bridge: a USI engine's process at one end, an ordinary CSA client at the
//! other.
//!
//! A preset registered with `protocol = "usi"` is a plain USI engine, which
//! cannot log in to anything. This module is what makes it a participant: the
//! server starts the engine as a child process, and the task here speaks USI
//! over its standard input and output while speaking CSA over a loopback
//! connection to the server's **own** listener, logging in with the entry's
//! token.
//!
//! A USI preset's games are ordinary games — login, pairing, rating, records and
//! the pages all see a normal participant — because from the server's side there
//! is a client on a socket, and this file may not invent a shortcut around that.
//!
//! Three tasks, on [`connection`](super::connection)'s terms: the two readers
//! each own their stream and forward whole lines over an `mpsc`, and the loop
//! that owns the game state selects over the two channels and the stop signal.
//! Reading a line is not cancel-safe — a cancelled read has already consumed
//! bytes — so no reader is ever a branch of a `select!` here.
//!
//! The bridge is a client: the server's clock, the legality of a move, the
//! repetition count and the outcome are all the server's. It keeps a position of
//! its own only to convert between two notations, and a clock of its own only
//! because a `go` line has to state one. It never ponders, and it handles one
//! game at a time.
//!
//! The clock the engine is given is derived from what the `Game_Summary` said
//! and from the T-values the server relays afterwards, by the derivation the
//! server itself makes: the number written as `,T` is the number deducted, so
//! `remaining + increment − T` is a settlement both sides compute and neither
//! transmits. Counting in `Time_Unit`s and converting to USI's milliseconds at
//! the `go` line keeps the two arithmetics apart.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::csa::{self, LineReader, LineWriter, WrittenMove};
use crate::game::{Color, Move, Position, StartSpec, apply_move};
use crate::usi::{self, BestMove, Clock, GameOver, Term};

use super::transport::Transport;

/// How long the engine is given to answer each step of its handshake.
///
/// Generous, because the step it is measured on is `isready`: an engine loading
/// a several-hundred-megabyte evaluation function answers that one late and is
/// not broken. The bound is for an engine that will never answer at all, which
/// would otherwise hold a slot for the life of the server.
const HANDSHAKE_PATIENCE: Duration = Duration::from_secs(120);

/// How many lines may queue from either side before the bridge stops reading.
///
/// A searching engine emits `info` lines faster than anything reads them, so
/// this is sized for a burst rather than for a backlog: when it fills, the
/// reader task waits, which is exactly the back-pressure a pipe would apply
/// anyway.
const LINES: usize = 256;

/// One preset engine to run, as the supervisor registers it.
///
/// No `Debug`: the token is credential material. What identifies the engine in a
/// log line is [`preset`](Self::preset), the index the operator's file writes it
/// at.
pub struct Engine {
    /// Which entry of `[matchmaking].preset_engine_tokens` this is, counted from
    /// zero — the identity every log line here carries.
    pub preset: usize,

    /// The token the bridge presents at `LOGIN`. Never passed to the child, and
    /// never written to a log.
    pub token: String,

    /// The engine's command: the program, then its arguments.
    pub command: Vec<String>,

    /// The USI options to set, already spelled as `setoption` carries them, in
    /// the order they are to be sent.
    pub options: Vec<(String, String)>,

    /// The `LOGIN` name the operator configured, or `None` for the engine's own
    /// `id name`.
    pub name: Option<String>,

    /// Where the listener is actually bound — the address dialled, which a
    /// loopback connection is expected to reach.
    pub address: SocketAddr,

    /// What the listener wraps a connection in, so the dial matches it.
    pub transport: Transport,
}

/// Runs one engine until it is stopped, its process exits, or its connection
/// ends.
///
/// Never returns an error to its caller: this is a spawned task, and the
/// supervisor learns that the preset is not playing from the task having
/// finished. Every failure is logged where it happens, at `error` when it is the
/// operator's configuration and at `warn` when it is the engine or the
/// connection.
pub async fn run(engine: Engine, stop: oneshot::Receiver<()>) {
    let preset = engine.preset;

    match play(engine, stop).await {
        Ok(Stopped::Asked) => info!(preset, "a USI preset engine's bridge stopped as asked"),
        Ok(Stopped::Ended) => {
            info!(
                preset,
                "a USI preset engine's bridge ended with its engine or its connection",
            );
        }
        // The operator's own file is at `error`, because nothing but an edit
        // will fix it; everything else is at `warn`, since a round will try
        // again.
        Err(
            error @ (Failure::NoCommand
            | Failure::Spawn(_)
            | Failure::Name { .. }
            | Failure::Login(_)),
        ) => {
            error!(preset, %error, "a USI preset engine cannot be run as registered");
        }
        Err(error) => warn!(preset, %error, "a USI preset engine's bridge ended"),
    }
}

/// Why the bridge came back without a failure.
enum Stopped {
    /// The supervisor asked it to stop.
    Asked,

    /// The connection closed, or the engine did.
    Ended,
}

/// Everything that can end a bridge other than being asked to.
#[derive(Debug, thiserror::Error)]
enum Failure {
    /// The command names no program at all. Refused at startup, so reaching this
    /// means the supervisor was handed an entry it should not have been.
    #[error("the command is empty")]
    NoCommand,

    /// The engine's process could not be started.
    #[error("the engine could not be started: {0}")]
    Spawn(#[source] std::io::Error),

    /// The engine did not answer a handshake step inside
    /// [`HANDSHAKE_PATIENCE`], or ended during it.
    #[error("the engine did not answer `{0}`")]
    Handshake(&'static str),

    /// The resolved `LOGIN` name is not one this server's listener accepts.
    #[error(
        "`{name}` is not a name this server's LOGIN accepts (letters, digits, `_@-.`, at most \
         1024 of them); write `name` on this preset's entry to give it one"
    )]
    Name {
        /// The name that was going to be presented.
        name: String,
    },

    /// The loopback connection could not be made, or failed later.
    #[error("the connection to this server's own listener failed: {0}")]
    Connection(#[source] std::io::Error),

    /// The server refused the login, which for a preset means the token in the
    /// entry is not the token the server is registering.
    #[error("the server answered the login with `{0}`")]
    Login(String),

    /// A line arrived that the bridge cannot act on. Fatal, because a bridge
    /// that guessed would be relaying a guess into a rated game.
    #[error("{0}")]
    Protocol(String),

    /// The engine's standard input or output went away.
    #[error("the engine's pipes closed")]
    Pipes,
}

/// The bridge proper.
async fn play(engine: Engine, mut stop: oneshot::Receiver<()>) -> Result<Stopped, Failure> {
    let preset = engine.preset;
    let (mut child, mut engine_in, engine_out) = spawn(&engine)?;
    // `kill_on_drop` is on the handle, so the child is killed whichever way this
    // function returns.
    let mut engine_out = BufReader::new(engine_out);

    let identified = handshake(&engine, &mut engine_in, &mut engine_out).await?;
    let name = match engine.name.clone().or(identified) {
        Some(name) if csa::is_engine_name(&name) => name,
        Some(name) => return Err(Failure::Name { name }),
        None => {
            return Err(Failure::Name {
                name: String::new(),
            });
        }
    };
    info!(preset, name, "a USI preset engine is ready and logging in");

    let stream = engine
        .transport
        .dial(engine.address)
        .await
        .map_err(Failure::Connection)?;
    let (reader, writer) = tokio::io::split(stream);
    let mut server = LineWriter::new(writer);

    // A reader blocked on a stream nobody will write to again would outlive this
    // task without the guard.
    let (lines, mut inbox) = mpsc::channel(LINES);
    let _readers = Readers(vec![
        tokio::spawn(read_server(
            LineReader::new(BufReader::new(reader)),
            lines.clone(),
        )),
        tokio::spawn(read_engine(engine_out, lines)),
    ]);

    write_server(&mut server, &format!("LOGIN {name} {}", engine.token)).await?;
    let mut state = State {
        preset,
        name,
        summary: Vec::new(),
        collecting: false,
        playing: None,
        stale: 0,
    };

    loop {
        let heard = tokio::select! {
            biased;

            // The stop wins a tie: a preset being stopped has nothing more to
            // say to the server.
            _ = &mut stop => {
                quit(&mut engine_in).await;
                return Ok(Stopped::Asked);
            }

            heard = inbox.recv() => heard,

            // Waited on here rather than polled by the supervisor, because this
            // task is the one that owns the handle.
            status = child.wait() => {
                match status {
                    Ok(status) => info!(preset, %status, "a USI preset engine's process exited"),
                    Err(error) => warn!(preset, %error, "a USI preset engine could not be waited for"),
                }
                return Ok(Stopped::Ended);
            }
        };

        let Some(heard) = heard else {
            return Ok(Stopped::Ended);
        };

        match heard {
            Heard::Server(line) => {
                state
                    .heard_from_server(&line, &mut server, &mut engine_in)
                    .await?;
            }
            Heard::Engine(line) => state.heard_from_engine(&line, &mut server).await?,
            Heard::Closed => return Ok(Stopped::Ended),
        }
    }
}

/// Starts the engine's process, with its input and output piped.
///
/// Standard error is inherited, so an engine that complains about its own
/// configuration complains where the server's own output goes.
fn spawn(engine: &Engine) -> Result<(Child, ChildStdin, tokio::process::ChildStdout), Failure> {
    let (program, arguments) = engine.command.split_first().ok_or(Failure::NoCommand)?;

    let mut child = Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        // A server that goes down takes its engines with it.
        .kill_on_drop(true)
        .spawn()
        .map_err(Failure::Spawn)?;

    let input = child.stdin.take().ok_or(Failure::Pipes)?;
    let output = child.stdout.take().ok_or(Failure::Pipes)?;

    Ok((child, input, output))
}

/// `usi` … `usiok`, the options, then `isready` … `readyok`.
///
/// Returns the engine's own `id name`, where it gave one: the default `LOGIN`
/// name. Which options an engine declares is not consulted — what to set is the
/// operator's statement.
async fn handshake(
    engine: &Engine,
    input: &mut ChildStdin,
    output: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<Option<String>, Failure> {
    write_engine(input, usi::USI).await?;

    let mut identified = None;
    loop {
        let line = expect_line(output, usi::USI).await?;
        if let Some(name) = usi::id_name(&line) {
            identified = Some(name.to_owned());
        }
        if line.trim() == usi::USIOK {
            break;
        }
    }

    for (name, value) in &engine.options {
        write_engine(input, &usi::setoption_line(name, value)).await?;
    }

    write_engine(input, usi::ISREADY).await?;
    loop {
        if expect_line(output, usi::ISREADY).await?.trim() == usi::READYOK {
            break;
        }
    }

    Ok(identified)
}

/// One line from the engine within [`HANDSHAKE_PATIENCE`].
async fn expect_line(
    output: &mut BufReader<tokio::process::ChildStdout>,
    step: &'static str,
) -> Result<String, Failure> {
    let mut line = String::new();
    let read = timeout(HANDSHAKE_PATIENCE, output.read_line(&mut line))
        .await
        .map_err(|_| Failure::Handshake(step))?
        .map_err(|_| Failure::Handshake(step))?;

    if read == 0 {
        return Err(Failure::Handshake(step));
    }

    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// What the loop heard, and from where.
enum Heard {
    /// One line from the server's listener.
    Server(String),

    /// One line from the engine.
    Engine(String),

    /// One of the two ended.
    Closed,
}

/// The reader tasks, aborted when the loop that owns them returns.
///
/// A reader blocked on a stream that will never be written to again would
/// otherwise linger holding a socket and a pipe for as long as the process runs.
struct Readers(Vec<JoinHandle<()>>);

impl Drop for Readers {
    fn drop(&mut self) {
        for reader in &self.0 {
            reader.abort();
        }
    }
}

/// Forwards the server's lines, and one [`Heard::Closed`] when they stop.
async fn read_server<R>(mut reader: LineReader<R>, lines: mpsc::Sender<Heard>)
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let heard = match reader.read_line().await {
            Ok(Some(line)) => Heard::Server(line.to_owned()),
            Ok(None) => Heard::Closed,
            Err(error) => {
                debug!(%error, "a bridge's connection to the listener failed");
                Heard::Closed
            }
        };
        let closing = matches!(heard, Heard::Closed);

        if lines.send(heard).await.is_err() || closing {
            return;
        }
    }
}

/// Forwards the engine's lines, and one [`Heard::Closed`] when they stop.
async fn read_engine(
    mut output: BufReader<tokio::process::ChildStdout>,
    lines: mpsc::Sender<Heard>,
) {
    loop {
        let mut line = String::new();
        let heard = match output.read_line(&mut line).await {
            Ok(0) | Err(_) => Heard::Closed,
            Ok(_) => Heard::Engine(line.trim_end_matches(['\r', '\n']).to_owned()),
        };
        let closing = matches!(heard, Heard::Closed);

        if lines.send(heard).await.is_err() || closing {
            return;
        }
    }
}

/// The bridge's own state: which preset it is, and the game it is in.
struct State {
    /// The entry's index, for the log lines.
    preset: usize,

    /// The name this bridge logged in under.
    name: String,

    /// The `Game_Summary` being read, `BEGIN` through `END`.
    ///
    /// Collected and read once it is whole rather than key by key as it arrives,
    /// because a summary that turns out to be unreadable must leave no
    /// half-built game behind.
    summary: Vec<String>,

    /// Whether [`summary`](Self::summary) is being collected right now.
    ///
    /// Not `!summary.is_empty()`: the lines of the last summary are still there
    /// after it was read, and a `BEGIN` is what starts a new one.
    collecting: bool,

    /// The game in progress, where there is one.
    playing: Option<Playing>,

    /// How many `bestmove`s the engine still owes to games that have ended.
    ///
    /// A game can end while the engine is searching, and the `go` it was given
    /// is owed an answer all the same.
    ///
    /// USI gives a `bestmove` no way to name the `go` it answers, so the engine
    /// answering its `go`s in order is what makes the next `bestmove` the oldest
    /// unanswered one. While this is not zero, that oldest one is a dead game's
    /// and is counted down rather than read.
    stale: usize,
}

/// One game, as the bridge has to keep it.
struct Playing {
    /// The `Game_ID`, so that a line about another game is recognised as one.
    game_id: String,

    /// Which side this engine plays.
    ours: Color,

    /// The position after every move relayed so far — what a move is resolved
    /// and rendered against.
    position: Position,

    /// Every move of the game in order, setup moves included, for the
    /// `position` line the engine is fed.
    moves: Vec<Move>,

    /// What each side has left of `Total_Time`, in `Time_Unit`s, `[black,
    /// white]`.
    remaining: [u32; 2],

    /// The time terms the summary announced.
    terms: Terms,

    /// Whether the engine has been asked for a move it has not answered.
    ///
    /// A `bestmove` that arrives when this is false is one for a game that has
    /// already ended, and is dropped rather than relayed. The same answer
    /// arriving after a later game has asked its own question is caught by
    /// [`State::stale`](State::stale) instead.
    thinking: bool,
}

/// The `Time` block, as a `go` line needs it.
struct Terms {
    /// How many milliseconds one `Time_Unit` is.
    unit: u64,

    /// `Increment`, in `Time_Unit`s.
    increment: u32,

    /// `Byoyomi`, in `Time_Unit`s.
    byoyomi: u32,
}

impl Terms {
    /// The clock as the engine is told it.
    ///
    /// The increment where there is one, and the byoyomi otherwise — including
    /// the `byoyomi 0` that says a clock which simply runs out. A game with both
    /// is announced as the increment, the term that applies to the move being
    /// asked for.
    fn clock(&self, remaining: [u32; 2]) -> Clock {
        Clock {
            black: u64::from(remaining[0]) * self.unit,
            white: u64::from(remaining[1]) * self.unit,
            term: if self.increment > 0 {
                Term::Increment(u64::from(self.increment) * self.unit)
            } else {
                Term::Byoyomi(u64::from(self.byoyomi) * self.unit)
            },
        }
    }
}

/// Which half of `remaining` a colour indexes.
const fn slot(color: Color) -> usize {
    match color {
        Color::Black => 0,
        Color::White => 1,
    }
}

impl State {
    /// One line from the server, and everything it can mean.
    ///
    /// The table is the CSA protocol read from the client's side: a summary to
    /// agree to, a `START` to begin play on, a move to apply, a status to end
    /// on. Anything else — an `##` warning, a keep-alive's empty line, a status
    /// this bridge has nothing to do about — is passed over, because a client
    /// that disconnected over a line it did not recognise would end a game it
    /// was winning.
    async fn heard_from_server<W>(
        &mut self,
        line: &str,
        server: &mut LineWriter<W>,
        engine: &mut ChildStdin,
    ) -> Result<(), Failure>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        if self.collecting {
            self.summary.push(line.to_owned());
            if line == "END Game_Summary" {
                self.collecting = false;
                return self.offered(server).await;
            }
            return Ok(());
        }

        match line {
            // The keep-alive answer, and the only empty line the server writes.
            "" => Ok(()),

            "BEGIN Game_Summary" => {
                self.summary.clear();
                self.summary.push(line.to_owned());
                self.collecting = true;
                Ok(())
            }

            _ if line.starts_with("LOGIN:") => {
                if line == format!("LOGIN:{} OK", self.name) {
                    info!(
                        preset = self.preset,
                        name = self.name,
                        "a USI preset engine logged in",
                    );
                    Ok(())
                } else {
                    Err(Failure::Login(line.to_owned()))
                }
            }

            _ if line.starts_with("START:") => self.started(line, engine).await,

            // Nothing was begun, so nothing is ended.
            _ if line.starts_with("REJECT:") => {
                debug!(
                    preset = self.preset,
                    line, "a bridged preset's pairing was rejected",
                );
                self.playing = None;
                Ok(())
            }

            // This engine's own move comes back here too, and is applied by the
            // same path the opponent's is, so the two boards cannot drift.
            _ if line.starts_with(['+', '-']) => self.relayed(line, engine).await,

            // A declaration echoed bare — `%KACHI` — or a resignation echoed
            // with its time. Neither changes the board, and the status lines
            // that follow are what end the game.
            _ if line.starts_with('%') => Ok(()),

            // `##[WARN] …`, which this bridge never provokes, taken before the
            // `#` arm below can mistake it for a status.
            _ if line.starts_with("##") => Ok(()),

            _ if line.starts_with('#') => self.status(line, engine).await,

            _ => Ok(()),
        }
    }

    /// A whole `Game_Summary` has arrived: read it, and agree.
    ///
    /// Agreement is unconditional: a preset exists to play whatever the round
    /// gives it.
    async fn offered<W>(&mut self, server: &mut LineWriter<W>) -> Result<(), Failure>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let playing = read_summary(&self.summary)?;
        info!(
            preset = self.preset,
            game = playing.game_id,
            black = playing.ours == Color::Black,
            "a bridged preset was offered a game",
        );
        self.playing = Some(playing);

        write_server(server, "AGREE").await
    }

    /// `START:<id>`: the game begins.
    ///
    /// `usinewgame` precedes the first `position` of every game, and the `go`
    /// follows immediately where this engine has the first move.
    async fn started(&mut self, line: &str, engine: &mut ChildStdin) -> Result<(), Failure> {
        let id = line.trim_start_matches("START:");
        let Some(playing) = self.playing.as_mut() else {
            return Err(Failure::Protocol(format!(
                "{line} began a game nothing offered"
            )));
        };
        if playing.game_id != id {
            return Err(Failure::Protocol(format!(
                "{line} began a game other than the offered {}",
                playing.game_id,
            )));
        }

        write_engine(engine, usi::USINEWGAME).await?;
        if playing.position.side_to_move() == playing.ours {
            return playing.think(engine).await;
        }

        Ok(())
    }

    /// A relayed move: apply it, charge its time, and think if the turn is now
    /// this engine's.
    async fn relayed(&mut self, line: &str, engine: &mut ChildStdin) -> Result<(), Failure> {
        let Some(playing) = self.playing.as_mut() else {
            // A move for a game this bridge is not in.
            return Ok(());
        };

        let (text, consumed) = timed(line)?;
        let written = WrittenMove::parse(text)
            .map_err(|error| Failure::Protocol(format!("{text}: {error}")))?;
        let mv = written
            .resolve(&playing.position)
            .map_err(|error| Failure::Protocol(format!("{text}: {error}")))?;
        let mover = playing.position.side_to_move();
        playing.position = apply_move(&playing.position, mv)
            .map_err(|error| Failure::Protocol(format!("{text}: {error}")))?;
        playing.moves.push(mv);
        playing.charge(mover, consumed);

        if playing.position.side_to_move() == playing.ours {
            return playing.think(engine).await;
        }

        Ok(())
    }

    /// A `#` status line. The three that end a game end it; the rest say why.
    async fn status(&mut self, line: &str, engine: &mut ChildStdin) -> Result<(), Failure> {
        let over = match line {
            "#WIN" => GameOver::Win,
            "#LOSE" => GameOver::Lose,
            // `#DRAW`, and `#CENSORED` for a game broken off with no result at
            // all: USI has no word for "no result", so the neutral one is used.
            "#DRAW" | "#CENSORED" => GameOver::Draw,
            // A reason line — `#RESIGN`, `#SENNICHITE`, `#CHUDAN` — which one of
            // the three above follows.
            _ => return Ok(()),
        };

        let Some(playing) = self.playing.take() else {
            return Ok(());
        };
        info!(
            preset = self.preset,
            game = playing.game_id,
            ending = line,
            "a bridged preset's game ended",
        );

        // A game can end while the engine is still searching, and the `go` is
        // then owed a `bestmove` belonging to a game that is over. `stop` asks
        // for it now, which rests on the protocol's contract that an engine told
        // `stop` answers promptly; the count does not rest on that, so an engine
        // that answers late still has the answer discarded rather than read as
        // the next game's.
        //
        // `stop` before `gameover`: an engine is not to be told a game is over
        // while it is still searching it.
        if playing.thinking {
            self.stale += 1;
            write_engine(engine, usi::STOP).await?;
        }

        write_engine(engine, over.as_str()).await
    }

    /// One line from the engine.
    ///
    /// Only `bestmove` is acted on. Everything else an engine says is read so
    /// that its output pipe never fills, and dropped.
    async fn heard_from_engine<W>(
        &mut self,
        line: &str,
        server: &mut LineWriter<W>,
    ) -> Result<(), Failure>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let Some(answer) = usi::parse_bestmove(line) else {
            return Ok(());
        };
        // The engine answers its `go`s in the order it was given them, so while
        // a dead game's answer is outstanding this is it: a move for a board
        // that no longer exists.
        if self.stale > 0 {
            self.stale -= 1;
            debug!(
                preset = self.preset,
                line, "a bridged preset answered a game that had already ended",
            );
            return Ok(());
        }
        // A `bestmove` with no game, or with no `go` outstanding, is a search
        // that finished after the server had already adjudicated.
        let Some(playing) = self.playing.as_mut() else {
            return Ok(());
        };
        if !playing.thinking {
            return Ok(());
        }
        playing.thinking = false;

        let text = match answer {
            BestMove::Resign => "%TORYO".to_owned(),
            BestMove::Win => "%KACHI".to_owned(),
            BestMove::Played(mv) => WrittenMove::of(playing.ours, mv, &playing.position)
                .map_err(|error| {
                    Failure::Protocol(format!("the engine's move cannot be written: {error}"))
                })?
                .to_string(),
        };

        write_server(server, &text).await
    }
}

impl Playing {
    /// Charges `consumed` to `mover`, the way the server charges it.
    ///
    /// `remaining + increment − T`, saturating at zero. The bridge recomputes it
    /// rather than being told, because the wire carries the deduction and not
    /// the remainder.
    fn charge(&mut self, mover: Color, consumed: u32) {
        let clock = &mut self.remaining[slot(mover)];
        *clock = clock
            .saturating_add(self.terms.increment)
            .saturating_sub(consumed);
    }

    /// Feeds the engine the position and asks it for a move.
    async fn think(&mut self, engine: &mut ChildStdin) -> Result<(), Failure> {
        write_engine(engine, &usi::position_line(&self.moves)).await?;
        write_engine(engine, &usi::go_line(&self.terms.clock(self.remaining))).await?;
        self.thinking = true;

        Ok(())
    }
}

/// A `<text>,T<n>` line split into its two halves.
fn timed(line: &str) -> Result<(&str, u32), Failure> {
    let refuse = || Failure::Protocol(format!("{line} is a move with no time"));

    let (text, consumed) = line.rsplit_once(",T").ok_or_else(refuse)?;

    Ok((text, consumed.parse().map_err(|_| refuse())?))
}

/// Reads a whole `Game_Summary` into the game the bridge will play.
///
/// The starting position is rebuilt from the block's setup moves, played from
/// hirate: a buoy `Position` block is hirate's rows and a move sequence, so the
/// line [`usi::position_line`] builds from them is the canonical `position
/// startpos moves …` line the server filed the game under, byte for byte.
fn read_summary(lines: &[String]) -> Result<Playing, Failure> {
    let refuse = |what: &str| Failure::Protocol(format!("the Game_Summary {what}"));

    let value = |key: &str| {
        let prefix = format!("{key}:");
        lines
            .iter()
            .find_map(|line| line.strip_prefix(&prefix))
            .map(str::to_owned)
    };
    let number = |key: &str| value(key).and_then(|written| written.parse::<u32>().ok());

    let game_id = value("Game_ID").ok_or_else(|| refuse("names no Game_ID"))?;
    let ours = match value("Your_Turn").as_deref() {
        Some("+") => Color::Black,
        Some("-") => Color::White,
        _ => return Err(refuse("names no side for this client")),
    };
    let unit = match value("Time_Unit").as_deref() {
        // The specification's default, for a server that omits the key. This one
        // never does.
        Some("1sec") | None => 1_000,
        Some("1min") => 60_000,
        Some("1msec") => 1,
        Some(other) => {
            return Err(refuse(&format!(
                "counts time in `{other}`, which is not a unit this bridge reads"
            )));
        }
    };
    let terms = Terms {
        unit,
        increment: number("Increment").unwrap_or(0),
        byoyomi: number("Byoyomi").unwrap_or(0),
    };

    // `Total_Time` omitted is the specification's "no limit", which USI cannot
    // state: an engine has to be given a number. This server always writes the
    // key, so the fallback is a clock of nothing, with whatever byoyomi the same
    // block announced carrying the game.
    let total = number("Total_Time").unwrap_or(0);
    let (moves, position, remaining) = read_position(lines, total, &terms)?;

    Ok(Playing {
        game_id,
        ours,
        position,
        moves,
        remaining,
        terms,
        thinking: false,
    })
}

/// The `Position` block: the board it writes, and the setup sequence it carries.
///
/// Returns the setup moves, the position they reach, and what each side has left
/// once their T-values have been charged.
///
/// A block whose board is not hirate is refused: accepting it by ignoring the
/// rows would have the engine play a different game from the one on the server's
/// board.
fn read_position(
    lines: &[String],
    total: u32,
    terms: &Terms,
) -> Result<(Vec<Move>, Position, [u32; 2]), Failure> {
    let block: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .skip_while(|line| *line != "BEGIN Position")
        .skip(1)
        .take_while(|line| *line != "END Position")
        .collect();

    // Asked of the encoder rather than spelled here, so the comparison is
    // against the very lines the other side of the connection produced.
    let hirate = csa::position_block::encode(&StartSpec::Buoy { setup: Vec::new() }, &[])
        .unwrap_or_else(|error| unreachable!("hirate is writable: {error}"));
    if block.len() < hirate.len() || block[..hirate.len()] != hirate[..] {
        return Err(Failure::Protocol(
            "the Game_Summary writes a board this bridge cannot start a USI game from: only the \
             buoy form — a hirate board and a setup sequence — is played here"
                .to_owned(),
        ));
    }

    let mut position = Position::hirate();
    let mut moves = Vec::new();
    let mut remaining = [total; 2];
    for line in &block[hirate.len()..] {
        let (text, consumed) = timed(line)?;
        let mv = WrittenMove::parse(text)
            .map_err(|error| Failure::Protocol(format!("{text}: {error}")))?
            .resolve(&position)
            .map_err(|error| Failure::Protocol(format!("{text}: {error}")))?;
        let mover = position.side_to_move();
        position = apply_move(&position, mv)
            .map_err(|error| Failure::Protocol(format!("{text}: {error}")))?;
        moves.push(mv);

        let clock = &mut remaining[slot(mover)];
        *clock = clock
            .saturating_add(terms.increment)
            .saturating_sub(consumed);
    }

    Ok((moves, position, remaining))
}

/// Writes one line to the engine, flushing it.
///
/// Flushed per line: an engine waiting on a `go` that is sitting in a buffer is
/// an engine being charged for the buffer.
async fn write_engine(engine: &mut ChildStdin, line: &str) -> Result<(), Failure> {
    engine
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|_| Failure::Pipes)?;
    engine.flush().await.map_err(|_| Failure::Pipes)
}

/// Writes one line to the server, flushing it.
async fn write_server<W>(server: &mut LineWriter<W>, line: &str) -> Result<(), Failure>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    server
        .write_line(line)
        .await
        .map_err(|error| Failure::Connection(std::io::Error::other(error.to_string())))?;
    server.get_mut().flush().await.map_err(Failure::Connection)
}

/// The last thing an engine being stopped is told.
///
/// Its failure is not reported: the child handle carries `kill_on_drop`, and a
/// pipe that is already closed is the state this was asking for.
async fn quit(engine: &mut ChildStdin) {
    if let Err(error) = write_engine(engine, usi::QUIT).await {
        debug!(%error, "a preset engine was already gone when it was told to quit");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::csa::game_summary::{self, GameSummary, TimeSettings, TimeUnit};

    /// A stand-in for the engine's standard input.
    ///
    /// The bridge writes to a `ChildStdin`, which only a real child process has,
    /// so the test gives it a real one: `cat`, which reads the lines and drops
    /// them. What the engine says is supplied by the test straight to
    /// [`State::heard_from_engine`].
    fn engine_pipe() -> (Child, ChildStdin) {
        let mut child = Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("`cat` runs");
        let input = child.stdin.take().expect("its standard input was piped");

        (child, input)
    }

    /// A `Game_Summary` offering `game_id` from hirate, with this bridge on
    /// Black — the side that is asked for a move the moment the game starts.
    fn offer(game_id: &str) -> Vec<String> {
        game_summary::encode(
            &GameSummary {
                game_id,
                black_name: "bridged",
                white_name: "outside",
                max_moves: None,
                time: TimeSettings {
                    unit: TimeUnit::Second,
                    total_time: Some(600),
                    byoyomi: 0,
                    increment: Some(1),
                    least_time_per_move: 0,
                    roundup: false,
                },
                start: &StartSpec::Buoy { setup: Vec::new() },
                setup_times: &[],
            },
            Color::Black,
        )
        .expect("hirate is writable")
    }

    /// A bridge with no game and nothing owed.
    fn fresh() -> State {
        State {
            preset: 0,
            name: "bridged".to_owned(),
            summary: Vec::new(),
            collecting: false,
            playing: None,
            stale: 0,
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_bestmove_owed_to_a_finished_game_is_not_relayed_into_the_next_one() {
        let (_process, mut engine) = engine_pipe();
        let mut server = LineWriter::new(Vec::new());
        let mut state = fresh();

        // Game one, as far as the `go` this bridge sends on its own first move.
        for line in offer("game-one") {
            state
                .heard_from_server(&line, &mut server, &mut engine)
                .await
                .expect("the summary is one this bridge plays");
        }
        state
            .heard_from_server("START:game-one", &mut server, &mut engine)
            .await
            .expect("the game starts");
        assert!(
            state
                .playing
                .as_ref()
                .is_some_and(|playing| playing.thinking),
            "the engine was asked for a move",
        );

        // Broken off while the engine is still searching: the preset-vs-preset
        // game giving up its slot to an external engine.
        for status in ["#CHUDAN", "#CENSORED"] {
            state
                .heard_from_server(status, &mut server, &mut engine)
                .await
                .expect("the game ends");
        }

        // Game two, as far as its own `go`.
        for line in offer("game-two") {
            state
                .heard_from_server(&line, &mut server, &mut engine)
                .await
                .expect("the second summary is one this bridge plays");
        }
        state
            .heard_from_server("START:game-two", &mut server, &mut engine)
            .await
            .expect("the second game starts");

        // Only now does game one's search finish. Against game two's board it
        // renders as a perfectly well-formed move, so nothing downstream of here
        // would catch it: it has to be caught here.
        let said = server.get_mut().clone();
        state
            .heard_from_engine("bestmove 2h3h", &mut server)
            .await
            .expect("a stale answer is not a failure");
        assert_eq!(
            *server.get_mut(),
            said,
            "game one's answer was relayed into game two",
        );

        // And game two's own answer, arriving behind it, is the one relayed.
        state
            .heard_from_engine("bestmove 7g7f", &mut server)
            .await
            .expect("the live answer is relayed");
        assert_eq!(
            String::from_utf8(server.get_mut().clone()).expect("the wire is UTF-8"),
            format!(
                "{}+7776FU\n",
                String::from_utf8(said).expect("the wire is UTF-8"),
            ),
        );
    }
}
