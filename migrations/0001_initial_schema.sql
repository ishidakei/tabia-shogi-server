-- The initial schema: every table and index this server stores, in one file.
--
-- Append-only from here on: `sqlx` checksums each file and compares the
-- checksums against the ones recorded in the database at every startup, so a
-- migration that has been applied to a deployment can no longer be edited.
--
-- There is no `ratings` table. A publication is a pure function of the `games`
-- rows and the moment it is made at, so a table would be a second durable copy
-- that can disagree with them; the latest publication is held in memory and
-- refitted at startup.

-- One row per finished game.
--
-- Both category tags are written at game creation and never derived from the
-- record afterwards. They are here from the first schema because a games table
-- that learned them later would have a prefix of its history untagged, and no
-- way to tag it.
--
-- `end_status` is the wire's status word without its `#`, with one exception: a
-- disconnect ends as a resignation on the wire, so a row carries `DISCONNECT`,
-- which is deliberately no CSA status at all, and the column is free text so
-- that it can.
--
-- `start_position` is the canonical USI `position` command string, stored
-- verbatim, with equality decided by full string comparison. A hash may be used
-- as an internal lookup aid wherever a hit is confirmed by comparing the whole
-- string; a digest alone is never proof that two positions are equal.
--
-- That column is nullable, and every row this server writes fills it in. A
-- `NULL` row has no line to measure, so its setup length is unknowable: it is
-- invisible to the matchmaker's statistics rather than counted under a default.
CREATE TABLE games (
  game_id          TEXT PRIMARY KEY,   -- the CSA Game_ID
  black_name       TEXT NOT NULL,      -- engine name at LOGIN
  white_name       TEXT NOT NULL,
  black_token_key  TEXT NOT NULL,      -- hex SHA-256 of the token presented at LOGIN
  white_token_key  TEXT NOT NULL,
  start_category   TEXT NOT NULL CHECK (start_category IN ('hirate','designated','handicap')),
  time_category    TEXT NOT NULL CHECK (time_category  IN ('symmetric','asymmetric')),
  started_at       TEXT NOT NULL,      -- RFC 3339, UTC
  ended_at         TEXT NOT NULL,      -- RFC 3339, UTC
  end_status       TEXT NOT NULL,      -- the game's status word, e.g. RESIGN, TIME_UP, DISCONNECT
  result           TEXT NOT NULL CHECK (result IN ('black','white','draw','none')),
  ply_count        INTEGER NOT NULL,   -- setup moves + played moves
  record_path      TEXT NOT NULL,      -- path of the .csa file, relative to the records directory
  start_position   TEXT                -- the canonical USI line the game started from
);

-- Newest-first is the one order every page that lists games reads them in.
CREATE INDEX games_ended_at ON games (ended_at);

-- The statistics query groups by this column at every matchmaking round, which
-- is the only read of it.
CREATE INDEX games_start_position ON games (start_position);

-- The participant page asks for the games of one token key, newest first, and
-- `games_ended_at` orders them but selects nothing.
--
-- Two indexes rather than one, because a participant is Black in some of its
-- games and White in the others, and the `black_token_key = ? OR
-- white_token_key = ?` that finds both is a query SQLite satisfies from one
-- index per branch.
CREATE INDEX games_black_token_key ON games (black_token_key);
CREATE INDEX games_white_token_key ON games (white_token_key);

-- One row per issued token.
--
-- `token_hash` is the same value `games.black_token_key` holds: the lowercase
-- hex SHA-256 of the token string presented at LOGIN, in both authentication
-- modes. That equivalence is what lets a token issued today match its own
-- earlier games with no translation table, and it is why the column is hex text
-- here as it is there rather than a blob.
--
-- The token itself is never stored.
--
-- A token is active exactly when `revoked_at IS NULL`, and an account's lifetime
-- count is its row count, so the two token caps need no stored counter. Nothing
-- is ever deleted, because a deleted row would be a lifetime slot freed, and
-- revocation frees only an active one.
--
-- `account_id` is the GitHub user id, and the primary key of `accounts` below.
-- There is no foreign key: the two tables are written by different halves of the
-- server, and a token's games outlive it.
--
-- `provisional_rating` is set at issuance if it is set at all, and read by the
-- matchmaking estimate for an engine that is not yet rated. It reaches neither
-- the rating fit nor a published table.
CREATE TABLE tokens (
  id                 INTEGER PRIMARY KEY,
  account_id         INTEGER NOT NULL,        -- the GitHub user id, as accounts.account_id
  token_hash         TEXT NOT NULL UNIQUE,    -- lowercase hex SHA-256, as games.*_token_key
  display_name       TEXT,                    -- the engine name most recently used at LOGIN
  provisional_rating INTEGER,                 -- the rating the matchmaking estimate uses, set at issuance or never
  issued_at          TEXT NOT NULL,           -- RFC 3339, UTC
  revoked_at         TEXT                     -- RFC 3339, UTC; NULL is an active token
);

-- The unique constraint above already indexes `token_hash`, which is the login
-- lookup; this is the other read -- an account's own rows, which the list page
-- renders and the issue path counts.
CREATE INDEX tokens_account_id ON tokens (account_id, id);

-- One row per GitHub account: what a sign-in stores, and the switch that decides
-- whether a visitor sees it.
--
-- The GitHub-derived data is exactly three fields, asserted by a schema test
-- over this table's column list so that a fourth identity column cannot appear
-- quietly.
--
-- `show_profile` is a tabia-side setting rather than GitHub data, and is one
-- switch over the whole profile: any one of the three items names the GitHub
-- account, from which the other two are public knowledge on github.com.
--
-- The default is owner-only, and it is the schema's rather than the inserting
-- code's, so a row written by a path that says nothing about visibility -- which
-- the sign-in write is -- leaks nothing.
--
-- SQLite has no boolean type. `sqlx` decodes `0`/`1` into `bool`, and the check
-- keeps a hand-edited database from producing a third state this code has no
-- branch for.
CREATE TABLE accounts (
  account_id   INTEGER PRIMARY KEY,     -- the GitHub user id, as tokens.account_id
  account_name TEXT NOT NULL,           -- the GitHub account name
  avatar_url   TEXT NOT NULL,           -- the URL of the profile image
  show_profile INTEGER NOT NULL DEFAULT 0 CHECK (show_profile IN (0, 1))
);

-- The designated rating of one engine that is not a preset, as an administrator
-- set it from the web.
--
-- The configuration file has no key for one: making a designation through
-- configuration would mean editing a file on the host and restarting the server,
-- which interrupts every game in progress. A preset's designated rating is not
-- here — it sits on the entry that registers the preset — and a row here that
-- names a preset's participant ID does not override it.
--
-- `token_key` is the participant ID: the lowercase hex SHA-256 of the token
-- presented at LOGIN, the same value `games.black_token_key` and
-- `tokens.token_hash` hold. It is the only identity this server has for an
-- engine that is not a preset, since it never holds such an engine's token.
--
-- One row per engine, so designating an engine that is already designated
-- replaces the value. There is no history table.
--
-- No foreign key and no existence check: a designation may name an engine that
-- has finished no game yet, in which case it does nothing until that engine is
-- rated.
--
-- The rating is an INTEGER with no CHECK: a rating scale has no zero to avoid
-- and no sign to insist on.
CREATE TABLE designated_ratings (
  token_key     TEXT PRIMARY KEY,  -- the participant ID, as games.black_token_key
  rating        INTEGER NOT NULL,  -- the designated rating
  designated_by INTEGER NOT NULL,  -- the GitHub user id of the administrator who set it
  designated_at TEXT NOT NULL      -- RFC 3339, UTC
);
