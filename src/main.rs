//! agent-relay — 別ホストで動く AI コーディングエージェント同士を中継する小さな MCP サーバ。
//!
//! # なぜ必要か
//!
//! Claude Code の `SendMessage` は同一セッションが起動したサブエージェント宛てで、
//! 別ホストのセッションとは通信できない。既存の類似 OSS はローカルの SQLite 共有が
//! 前提で、マシンを跨げない。そこを埋める。
//!
//! # 方式
//!
//! MCP の Streamable HTTP transport（仕様 2025-06-18）のうち、単一 JSON レスポンスで
//! 完結する部分だけを実装する。SSE ストリームは開かない（GET には 405 を返す）。
//! 受信は poll 型。エージェントはツール実行のタイミングでしか受信できないため、
//! サーバから一方的に流す意味が薄い。
//!
//! # 安全側の既定
//!
//! - 既定で **127.0.0.1 のみ**を listen する（仕様の Security Warning に従う）。
//!   別ホストからは SSH ポートフォワード経由で繋ぐ想定
//! - `Origin` ヘッダがある場合は検証する（DNS リバインディング対策）

mod bus;
mod mcp;
mod store;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use bus::Bus;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use store::Store;

/// 現在の UNIX 秒。ピアの生存判定に使う。
///
/// 時計が UNIX_EPOCH より前を指す異常時は 0 に倒す。失効判定が働かなくなるだけで、
/// サーバを落とすほどのことではない。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone)]
struct AppState {
    bus: Arc<Mutex<Bus>>,
    /// 受信箱の保存先。未設定ならメモリのみ（従来の挙動）
    store: Arc<Option<Store>>,
    /// 既定（localhost）に加えて許可する Origin。リバースプロキシ経由の公開用。
    allowed_origins: Arc<Vec<String>>,
}

#[tokio::main]
async fn main() {
    // 既定は localhost のみ。外部公開したい場合だけ明示的に上書きさせる
    let addr = std::env::var("AGENT_RELAY_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".to_string());

    // 参加の合言葉。公開ホストへ置くなら設定する。未設定なら誰でも参加できる
    let join_secret = std::env::var("AGENT_RELAY_JOIN_SECRET").ok();
    // リバースプロキシ越しに公開する場合、そのホスト名を許可する必要がある
    let allowed_origins: Vec<String> = std::env::var("AGENT_RELAY_ALLOWED_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // 受信箱の保存先。未設定ならメモリのみで、従来どおり再起動で未読は消える。
    // 相手が不在の間の取り置きを再起動またぎで効かせたい場合に設定する
    let store = std::env::var("AGENT_RELAY_STATE_FILE")
        .ok()
        .filter(|p| !p.is_empty())
        .map(Store::new);

    let mut bus = Bus::with_join_secret(join_secret);
    if let Some(st) = &store {
        let restored = st.load();
        let count: usize = restored.values().map(Vec::len).sum();
        bus.restore_inboxes(restored);
        println!(
            "受信箱を {} に保存します（復元した未読 {count} 件）",
            st.path().display()
        );
    }
    if bus.requires_secret() {
        println!("参加には合言葉が必要です (AGENT_RELAY_JOIN_SECRET)");
    } else {
        println!("警告: 合言葉が未設定です。到達できる相手は誰でも参加できます");
    }
    if !allowed_origins.is_empty() {
        println!("許可する Origin: {}", allowed_origins.join(", "));
    }

    let state = AppState {
        bus: Arc::new(Mutex::new(bus)),
        store: Arc::new(store),
        allowed_origins: Arc::new(allowed_origins),
    };

    let app = Router::new()
        .route("/mcp", any(mcp_endpoint))
        .route("/healthz", any(healthz))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("listen できません ({addr}): {e}");
            std::process::exit(1);
        }
    };

    println!("agent-relay listening on http://{addr}/mcp");
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("サーバが停止しました: {e}");
        std::process::exit(1);
    }
}

/// SIGINT / SIGTERM で片付けて終える。systemd から stop されたときに素直に落ちるため。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    println!("shutting down");
}

async fn healthz(State(state): State<AppState>) -> Response {
    let (peers, pending) = {
        let mut bus = state.bus.lock().expect("bus lock");
        // 失効済みを数に含めない。監視から見て「生きているピア数」であってほしい
        bus.sweep(now_secs());
        (bus.peers().len(), bus.pending_count())
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        format!(r#"{{"status":"ok","peers":{peers},"pending":{pending}}}"#),
    )
        .into_response()
}

async fn mcp_endpoint(
    State(state): State<AppState>,
    method: axum::http::Method,
    headers: HeaderMap,
    body: String,
) -> Response {
    // DNS リバインディング対策。Origin が付いている場合のみ検証する
    // （curl や MCP クライアントは付けてこないことが多い）
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && !is_allowed_origin(origin)
        && !state.allowed_origins.iter().any(|o| o == origin)
    {
        return (StatusCode::FORBIDDEN, "Origin が許可されていません").into_response();
    }

    match method {
        axum::http::Method::POST => {}
        // SSE ストリームは提供しない。仕様上 405 を返してよい
        axum::http::Method::GET => {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                "SSE ストリームは提供しません",
            )
                .into_response();
        }
        // セッションを持たないので DELETE も受け付けない
        axum::http::Method::DELETE => {
            return (StatusCode::METHOD_NOT_ALLOWED, "セッション管理は行いません").into_response();
        }
        _ => return (StatusCode::METHOD_NOT_ALLOWED, "POST を使ってください").into_response(),
    }

    let (outcome, snapshot) = {
        let mut bus = state.bus.lock().expect("bus lock");
        let outcome = mcp::handle(&mut bus, &body, now_secs());
        // 中身が変わったときだけ書く。shim は2秒間隔で叩いてくるので、
        // 毎回書くと何も起きていない時間帯もディスクを回し続けることになる
        let snapshot = if state.store.is_some() && bus.take_dirty() {
            Some(bus.inboxes().clone())
        } else {
            None
        };
        (outcome, snapshot)
    };

    if let (Some(st), Some(snapshot)) = (state.store.as_ref(), snapshot)
        && let Err(e) = st.save(&snapshot)
    {
        // 保存に失敗しても中継そのものは続ける。届けることを優先する
        eprintln!("受信箱を保存できませんでした: {e}");
    }

    match outcome {
        mcp::Outcome::Reply(json) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        // 仕様: notification / response には 202 を返し body は空にする
        mcp::Outcome::Accepted => StatusCode::ACCEPTED.into_response(),
    }
}

/// ローカル由来の Origin だけを許す。
///
/// 前方一致で判定すると `http://localhost.evil.example.com` のような
/// サブドメイン偽装を通してしまう。スキームを剥がしてホスト名を取り出し、
/// ポートを除いた部分の**完全一致**で判定する。
fn is_allowed_origin(origin: &str) -> bool {
    let rest = match origin.split_once("://") {
        Some(("http" | "https", rest)) => rest,
        _ => return false,
    };
    // パスがあれば落とし、ポートを分離する。IPv6 の [::1] にも対応する
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        // [::1]:8765 の形
        Some(after) => match after.split_once(']') {
            Some((h, _port)) => h,
            None => return false,
        },
        None => authority.split(':').next().unwrap_or(""),
    };

    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ローカル由来のoriginだけ許す() {
        assert!(is_allowed_origin("http://127.0.0.1:8765"));
        assert!(is_allowed_origin("http://localhost:3000"));
        assert!(is_allowed_origin("https://localhost"));
        assert!(is_allowed_origin("http://127.0.0.1"));
        // IPv6 のループバック
        assert!(is_allowed_origin("http://[::1]:8765"));
    }

    #[test]
    fn 外部のoriginは拒否する() {
        // DNS リバインディングで外部サイトから叩かれるのを防ぐ
        assert!(!is_allowed_origin("http://evil.example.com"));
        assert!(!is_allowed_origin("https://attacker.test"));
    }

    #[test]
    fn ホスト名の偽装を弾く() {
        // 前方一致で判定すると通ってしまう形。完全一致で判定していることの担保
        assert!(!is_allowed_origin("http://localhost.evil.example.com"));
        assert!(!is_allowed_origin("http://127.0.0.1.evil.example.com"));
        assert!(!is_allowed_origin("http://localhostx"));
        // 認証情報を挟んで localhost に見せかける形
        assert!(!is_allowed_origin("http://localhost@evil.example.com"));
        // スキームが http/https でないもの
        assert!(!is_allowed_origin("file://localhost"));
        assert!(!is_allowed_origin("localhost:8765"));
        assert!(!is_allowed_origin(""));
    }
}
