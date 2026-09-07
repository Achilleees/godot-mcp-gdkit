//! HTTP Transport
//!
//! Opt-in Streamable-HTTP serving of the same tool router the stdio transport uses. Stdio
//! stays the default; HTTP exists for a remote MCP client. Fail-closed by contract: HTTP
//! enabled without a bearer token refuses to start — there is never an open listener. Every
//! request without the exact `Authorization: Bearer <token>` header gets a bare 401.
//!
//! Enablement: the `--http [addr]` flag, and only the flag — deliberately no ambient env
//! switch, because plugin-launched stdio instances inherit the environment (via `.mcp.json`)
//! and an exported enable-var would silently flip them to HTTP. The bind address defaults to
//! loopback (127.0.0.1:8642); `--bind <addr>` widens it (remote clients). The token comes
//! from `--token <tok>` or `GDKIT_HTTP_TOKEN` (flag wins). Addresses accept `ip:port` or a
//! bare port (bound on 127.0.0.1). The MCP endpoint is mounted at `/mcp`.

use std::future::IntoFuture;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use crate::server::GdkitServer;

/// Default listen address when `--http` is given without one: loopback, never wide open.
const DEFAULT_ADDR: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 8642);

/// How the server talks MCP this run. Resolved once at startup from argv + env.
#[derive(Debug, PartialEq)]
pub enum Transport {
    /// The default: JSON-RPC over stdin/stdout, exactly as when no flag is given.
    Stdio,
    Http(HttpOptions),
}

#[derive(PartialEq)]
pub struct HttpOptions {
    pub bind: SocketAddr,
    /// The exact bearer token every request must present. Never empty — resolution refuses
    /// to produce `Http` without one (fail closed).
    pub token: String,
}

/// Manual impl so the token can never reach a log line — any `{:?}` of the transport prints
/// `<redacted>` in its place.
impl std::fmt::Debug for HttpOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpOptions")
            .field("bind", &self.bind)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Resolve the transport from CLI args (argv without the program name) and an env lookup.
/// The lookup is injected so tests never touch process-global env state.
///
/// Errors are startup-fatal by design: an unknown flag, a malformed address, `--bind`/`--token`
/// without HTTP enabled, and — the fail-closed core — HTTP enabled with no token.
pub fn resolve_transport(
    args: &[String],
    env: impl Fn(&str) -> Option<String>,
) -> Result<Transport, String> {
    let mut http_flag = false;
    let mut http_addr: Option<SocketAddr> = None;
    let mut bind_addr: Option<SocketAddr> = None;
    let mut token_flag: Option<String> = None;

    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--http" => {
                http_flag = true;
                // Optional value: the next arg, when it isn't itself a flag.
                if let Some(next) = it.peek() {
                    if !next.starts_with("--") {
                        http_addr = Some(parse_addr(next)?);
                        it.next();
                    }
                }
            }
            "--bind" => {
                let v = it.next().ok_or("--bind requires an address")?;
                bind_addr = Some(parse_addr(v)?);
            }
            "--token" => {
                let v = it.next().ok_or("--token requires a value")?;
                token_flag = Some(v.clone());
            }
            other => {
                return Err(format!(
                    "unknown argument: {other}\nusage: godot-mcp [--http [addr]] [--bind addr] [--token tok]"
                ));
            }
        }
    }

    // The flag is the only switch (see the module header for why no env enablement exists).
    if !http_flag {
        if bind_addr.is_some() {
            return Err("--bind requires --http".into());
        }
        if token_flag.is_some() {
            return Err("--token requires --http".into());
        }
        return Ok(Transport::Stdio);
    }

    // Address precedence: --bind > --http's addr > the loopback default.
    let bind = bind_addr.or(http_addr).unwrap_or(DEFAULT_ADDR);

    // Fail closed: no token, no listener.
    let token = token_flag
        .or_else(|| env("GDKIT_HTTP_TOKEN"))
        .filter(|t| !t.is_empty())
        .ok_or("HTTP transport enabled but no token set — refusing to start an open listener. Set GDKIT_HTTP_TOKEN or pass --token.")?;

    Ok(Transport::Http(HttpOptions { bind, token }))
}

/// `ip:port`, or a bare port bound on loopback.
fn parse_addr(s: &str) -> Result<SocketAddr, String> {
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(port) = s.parse::<u16>() {
        return Ok(SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        ));
    }
    Err(format!(
        "invalid address {s:?} — expected ip:port or a bare port"
    ))
}

/// The full HTTP app: the rmcp Streamable-HTTP service at `/mcp`, a bare-404 fallback, and
/// the bearer gate wrapped around both — so a request to ANY path without the token is 401.
pub fn router(server: GdkitServer, opts: &HttpOptions) -> Router {
    let mut config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(Some(std::time::Duration::from_secs(30)));
    if !opts.bind.ip().is_loopback() {
        // rmcp's default Host allowlist is loopback names only; a widened bind is reached via
        // client addresses or DNS names not known in advance, so the check comes off. The bearer
        // gate is the auth boundary — DNS-rebinding pages cannot attach the token.
        config = config.disable_allowed_hosts();
    }

    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    Router::new()
        .nest_service("/mcp", mcp)
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn_with_state(
            Arc::new(opts.token.clone()),
            require_bearer,
        ))
}

/// The bearer gate: exact-token match or a bare 401 — no body detail, no header hints.
async fn require_bearer(
    State(expected): State<Arc<String>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let ok = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|presented| ct_eq(presented.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Constant-time byte equality — comparison cost is independent of where a mismatch sits.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

/// Bind and serve until Ctrl-C. The caller kills Godot children afterwards (same contract as
/// the stdio path's shutdown).
pub async fn serve_http(server: GdkitServer, opts: HttpOptions) -> anyhow::Result<()> {
    let app = router(server, &opts);
    let listener = tokio::net::TcpListener::bind(opts.bind).await?;
    tracing::info!("http transport listening on http://{}/mcp", opts.bind);

    tokio::select! {
        r = axum::serve(listener, app).into_future() => { r?; }
        _ = tokio::signal::ctrl_c() => { tracing::info!("ctrl-c, shutting down"); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{header::AUTHORIZATION, Request, StatusCode};
    use tower::ServiceExt;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    // ---- transport resolution ------------------------------------------------------------

    #[test]
    fn no_args_is_stdio() {
        assert_eq!(resolve_transport(&[], no_env), Ok(Transport::Stdio));
    }

    #[test]
    fn token_env_alone_leaves_stdio() {
        // GDKIT_HTTP_TOKEN without --http leaves stdio untouched.
        let env = |k: &str| (k == "GDKIT_HTTP_TOKEN").then(|| "tok".to_string());
        assert_eq!(resolve_transport(&[], env), Ok(Transport::Stdio));
    }

    #[test]
    fn http_without_token_refuses_to_start() {
        assert!(resolve_transport(&s(&["--http"]), no_env).is_err());
    }

    #[test]
    fn http_with_empty_token_refuses_to_start() {
        let env = |k: &str| (k == "GDKIT_HTTP_TOKEN").then(String::new);
        assert!(resolve_transport(&s(&["--http"]), env).is_err());
    }

    #[test]
    fn http_defaults_to_loopback() {
        let env = |k: &str| (k == "GDKIT_HTTP_TOKEN").then(|| "tok".to_string());
        let t = resolve_transport(&s(&["--http"]), env).unwrap();
        assert_eq!(
            t,
            Transport::Http(HttpOptions {
                bind: "127.0.0.1:8642".parse().unwrap(),
                token: "tok".into()
            })
        );
    }

    #[test]
    fn http_takes_addr_or_bare_port() {
        let env = |k: &str| (k == "GDKIT_HTTP_TOKEN").then(|| "tok".to_string());
        let t = resolve_transport(&s(&["--http", "127.0.0.1:9000"]), env).unwrap();
        assert_eq!(
            t,
            Transport::Http(HttpOptions {
                bind: "127.0.0.1:9000".parse().unwrap(),
                token: "tok".into()
            })
        );
        let t = resolve_transport(&s(&["--http", "9001"]), env).unwrap();
        assert_eq!(
            t,
            Transport::Http(HttpOptions {
                bind: "127.0.0.1:9001".parse().unwrap(),
                token: "tok".into()
            })
        );
    }

    #[test]
    fn bind_overrides_and_widens() {
        let t = resolve_transport(
            &s(&["--http", "--bind", "0.0.0.0:9000", "--token", "tok"]),
            no_env,
        )
        .unwrap();
        assert_eq!(
            t,
            Transport::Http(HttpOptions {
                bind: "0.0.0.0:9000".parse().unwrap(),
                token: "tok".into()
            })
        );
    }

    #[test]
    fn token_flag_beats_env() {
        let env = |k: &str| (k == "GDKIT_HTTP_TOKEN").then(|| "env-tok".to_string());
        let t = resolve_transport(&s(&["--http", "--token", "flag-tok"]), env).unwrap();
        match t {
            Transport::Http(o) => assert_eq!(o.token, "flag-tok"),
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn ambient_env_never_enables_http() {
        // A GDKIT_HTTP var inherited by a plugin-launched stdio instance (via `.mcp.json`)
        // must be inert — the explicit flag is the only switch.
        let env = |k: &str| match k {
            "GDKIT_HTTP" => Some("127.0.0.1:9100".to_string()),
            "GDKIT_HTTP_TOKEN" => Some("tok".to_string()),
            _ => None,
        };
        assert_eq!(resolve_transport(&[], env), Ok(Transport::Stdio));
    }

    #[test]
    fn debug_redacts_token() {
        let opts = HttpOptions {
            bind: "127.0.0.1:8642".parse().unwrap(),
            token: "sekrit".into(),
        };
        let printed = format!("{opts:?}");
        assert!(!printed.contains("sekrit"));
        assert!(printed.contains("<redacted>"));
    }

    #[test]
    fn bind_or_token_without_http_errors() {
        assert!(resolve_transport(&s(&["--bind", "0.0.0.0:9000"]), no_env).is_err());
        assert!(resolve_transport(&s(&["--token", "tok"]), no_env).is_err());
    }

    #[test]
    fn unknown_arg_errors() {
        assert!(resolve_transport(&s(&["--htpp"]), no_env).is_err());
    }

    // ---- bearer gate ---------------------------------------------------------------------

    fn test_router() -> Router {
        let opts = HttpOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: "secret".into(),
        };
        router(GdkitServer::new(crate::config::Config::default()), &opts)
    }

    async fn hit(router: Router, path: &str, auth: Option<&str>) -> (StatusCode, usize) {
        let mut req = Request::get(path);
        if let Some(a) = auth {
            req = req.header(AUTHORIZATION, a);
        }
        let res = router
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let body = to_bytes(res.into_body(), 1 << 16).await.unwrap();
        (status, body.len())
    }

    #[tokio::test]
    async fn missing_token_is_bare_401_on_every_path() {
        for path in ["/mcp", "/", "/anything"] {
            let (status, body_len) = hit(test_router(), path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "path {path}");
            assert_eq!(body_len, 0, "401 must carry no body detail (path {path})");
        }
    }

    #[tokio::test]
    async fn wrong_token_is_bare_401() {
        let (status, body_len) = hit(test_router(), "/mcp", Some("Bearer wrong")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body_len, 0);
        // Same for a non-Bearer scheme.
        let (status, _) = hit(test_router(), "/mcp", Some("Basic secret")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn right_token_passes_the_gate() {
        // /mcp: rmcp answers the GET (405/406/40x depending on headers) — anything but 401
        // proves the gate opened and the request reached the MCP service.
        let (status, _) = hit(test_router(), "/mcp", Some("Bearer secret")).await;
        assert_ne!(status, StatusCode::UNAUTHORIZED);
        // Unknown path with the right token: the 404 fallback, not the gate.
        let (status, _) = hit(test_router(), "/nope", Some("Bearer secret")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn ct_eq_matches_exactly() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
        assert!(ct_eq(b"", b""));
    }
}
