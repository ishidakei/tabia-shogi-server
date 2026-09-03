//! Sign-in at the socket: what a running server serves when a visitor asks to
//! sign in, and what it does not serve when there is nothing to sign in to.
//!
//! The flow itself is not here: a whole sign-in needs a GitHub to answer, and
//! the endpoints are in the source rather than in a configuration key
//! (`services::sso::Endpoints`) so that an operator cannot point the token
//! endpoint at another host. Those tests drive the real router against a
//! substituted GitHub, in `src/web/sso.rs`.
//!
//! What is here is what only a running server can show: that `Startup` → `run` →
//! `web::serve` wires the sign-in half, that the redirect carries the configured
//! client id and no scope, that the `401` an unsigned-in visitor gets offers a
//! way in, and that an `open`-mode instance serves none of it.

mod common;

use common::{
    HIRATE, OAUTH_TABLE, PROMPT_SCHEDULE, Records, config_text, fetch, start, start_with_sso,
    storage_lines,
};

/// The client id the `github`-mode configuration below names.
///
/// `common::OAUTH_TABLE`'s, written out here as well: an assertion against the
/// constant would pass whatever the constant said.
const CLIENT_ID: &str = "Iv23li-a-test-client-id";

/// A `github`-mode configuration with its OAuth app.
///
/// `common::config_text` is `open` mode's. This is the same shape with the key
/// that decides what a `LOGIN` is verified against, and the table a
/// `github`-mode instance must carry to start at all.
fn github_config() -> String {
    format!(
        "\
auth_mode = \"github\"
positions = \"tests/fixtures/positions/hirate.txt\"
{storage}
{PROMPT_SCHEDULE}
[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = 600
increment = 0
least_time_per_move = 1
roundup = false

[web]
host = \"127.0.0.1\"
port = 0
{OAUTH_TABLE}",
        storage = storage_lines(),
    )
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_github_instance_sends_a_visitor_to_github_with_its_client_id_and_no_scope() {
    // The production wiring, over a real socket: the configuration an operator
    // writes, through `Startup` and `run`, to a redirect a browser would follow.
    let config = github_config();
    let _records = Records::of(&config);
    let server = start_with_sso(&config, HIRATE).await;
    let web = server.web_addr();

    let started = fetch(web, "/sign-in").await;

    assert_eq!(started.status, 303);

    let location = location_of(web, "/sign-in").await;
    assert!(
        location.starts_with("https://github.com/login/oauth/authorize?"),
        "{location}"
    );
    assert!(
        location.contains(&format!("client_id={CLIENT_ID}")),
        "{location}"
    );
    assert!(location.contains("&state="), "{location}");
    // The default grant is the public profile, so the way to ask for nothing
    // more than an identity is to ask for no scope at all.
    assert!(!location.contains("scope"), "{location}");
    // And the client secret is nowhere near a redirect a browser follows.
    assert!(!location.contains("secret"), "{location}");

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_attempt_arrives_with_a_signed_cookie_carrying_every_attribute() {
    let config = github_config();
    let _records = Records::of(&config);
    let server = start_with_sso(&config, HIRATE).await;
    let web = server.web_addr();

    let set_cookie = header_of(web, "/sign-in", "set-cookie").await;

    assert!(set_cookie.starts_with("tabia_session="), "{set_cookie}");
    for attribute in ["Path=/", "HttpOnly", "Secure", "SameSite=Lax", "Max-Age="] {
        assert!(set_cookie.contains(attribute), "{attribute}: {set_cookie}");
    }
    // The value is the opaque id and its MAC: two runs of 64 lowercase hex
    // characters with a dot between them, and nothing that says who it is.
    let value = set_cookie
        .strip_prefix("tabia_session=")
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| panic!("{set_cookie}"));
    let (id, mac) = value
        .split_once('.')
        .unwrap_or_else(|| panic!("the cookie is not signed: {value}"));
    for half in [id, mac] {
        assert_eq!(half.len(), 64, "{value}");
        assert!(
            half.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{value}"
        );
    }

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_signed_in_page_refuses_a_visitor_and_offers_them_the_sign_in() {
    // The refusal is an answer rather than a notice: it carries the route a
    // visitor signs in through.
    let config = github_config();
    let _records = Records::of(&config);
    let server = start_with_sso(&config, HIRATE).await;
    let web = server.web_addr();

    for path in ["/tokens", "/account"] {
        let refused = fetch(web, path).await;

        assert_eq!(refused.status, 401, "{path}");
        refused.assert_contains("href=\"/sign-in\"");
    }

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn an_open_instance_serves_no_sign_in_and_everything_else_as_before() {
    // The `open`-mode half of the last criterion. There are no accounts here —
    // every game runs with none and the token store is never read — so a session
    // would be an identity with nothing to be the identity of, and the routes
    // are simply not served.
    let config = config_text(4, 1);
    let _records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();

    for path in ["/sign-in", "/sign-in/callback", "/sign-out"] {
        assert_eq!(fetch(web, path).await.status, 404, "{path}");
    }

    // And the spectator pages, which an `open` instance serves in full.
    let listing = fetch(web, "/").await;
    assert_eq!(listing.status, 200);
    listing.assert_contains("tabia-shogi-server");
    for path in ["/participants", "/ratings", "/ratings/recent"] {
        assert_eq!(fetch(web, path).await.status, 200, "{path}");
    }
    // Including the `401` on the signed-in ones, which no cookie can move here:
    // there is no middleware to read one.
    assert_eq!(fetch(web, "/tokens").await.status, 401);

    server.shutdown().await;
}

/// The `Location` header of the answer to `GET path`.
async fn location_of(web: std::net::SocketAddr, path: &str) -> String {
    header_of(web, path, "location").await
}

/// One header of the answer to `GET path`.
///
/// `common::fetch` parses the status, the content type and the body, which is
/// what a page test needs; a redirect's whole content is in a header, so this
/// reads the raw answer the way `common::fetch_raw` exists to allow.
async fn header_of(web: std::net::SocketAddr, path: &str, name: &str) -> String {
    let raw = common::fetch_raw(web, path, common::PATIENCE).await;
    let (head, _) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in {raw:?}"));

    head.lines()
        .skip(1)
        .find_map(|line| {
            let (header, value) = line.split_once(':')?;
            header
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_else(|| panic!("no {name} in {head:?}"))
}
