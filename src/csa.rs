//! CSA protocol layer: bytes to typed commands and back.
//!
//! This is the only layer that knows what a CSA line looks like. Everything
//! arriving from a socket is bounded here, before it is allocated on.

pub mod codec;
pub mod command;
pub mod game_summary;
pub mod notation;
pub mod position_block;
pub mod response;

// `position_block::Error` is deliberately not re-exported: `csa::Error` is the
// codec's, and a second one at this root could only be renamed into a stutter.
pub use codec::{Error, LineReader, LineWriter, MAX_LINE_LEN};
pub use command::{Command, Commented, LoginRejection, Unparsed, split_comment};
pub use game_summary::{GameSummary, TimeSettings, TimeUnit};
pub use notation::{Endpoint, ParseError, RenderError, ResolveError, WrittenMove};
pub use response::{GameResult, MoveEcho, Reason, Response, Termination};
