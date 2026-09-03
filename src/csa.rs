//! CSA protocol layer: bytes to typed commands and back.
//!
//! Everything arriving from a socket is bounded here, before it is allocated
//! on.

pub mod codec;
pub mod command;
pub mod game_summary;
pub mod notation;
pub mod position_block;
pub mod record;
pub mod response;

pub use codec::{Error, LineReader, LineWriter, MAX_LINE_LEN};
pub use command::{Command, Commented, LoginRejection, Unparsed, is_engine_name, split_comment};
pub use game_summary::{GameSummary, TimeSettings, TimeUnit};
pub use notation::{Endpoint, ParseError, RenderError, ResolveError, WrittenMove, letters_of};
pub use record::{Ending, Played};
pub use response::{Closing, GameResult, MoveEcho, Reason, Response, Termination};
