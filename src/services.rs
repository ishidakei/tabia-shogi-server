//! The web service layer: the web half's logic, free of HTTP.
//!
//! Nothing here names an HTTP type, so assembling a game list or reading a
//! record is exercised without constructing a request.
//!
//! [`Context`] is the layer's front door, so the layer above holds one value
//! and names no storage.
//!
//! The view models are constructed here and nowhere else. Every type [`games`]
//! returns has private fields and accessors, so a page cannot render an
//! unfiltered account by forgetting to filter: there is no other way to obtain
//! one.

pub mod board;
pub mod context;
pub mod designations;
pub mod games;
pub mod participants;
pub mod privacy;
pub mod rating;
pub mod record;
pub mod snapshot;
pub mod sso;
pub mod tokens;

pub use board::{Board, Cell, Row};
pub use context::{Context, Error};
pub use designations::{
    Administration, Candidate, Designating, DesignationEntry, DesignationRefusal, DesignationsPage,
};
pub use games::{FinishedEntry, FinishedGame, GamePage, Listing, LiveEntry, LiveGame, PAGE};
pub use participants::{Participant, ParticipantEntry, Participants};
pub use privacy::{AccountSettings, Profiles, PublicProfile};
pub use rating::{
    Floodgate, Origin, Publication, Publications, RatedGame, RatedOutcome, RatedParticipant,
    RatingEntry, RatingSystem, RatingTable, RatingTablePage, RatingTables, Ratings, Scale,
    ScaleSource, Timestamp, Unrated, Window,
};
pub use record::{ReadError, read};
pub use snapshot::{GameSnapshot, Live, Registry};
pub use sso::{Endpoints, GitHubOAuth, GithubUser, SsoError};
pub use tokens::{
    AccountId, Accounts, Capping, Caps, Issue, Issued, Refusal, TokenEntry, TokenId, TokenList,
};
