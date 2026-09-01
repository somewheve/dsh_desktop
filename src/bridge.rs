//! 本地桥接服务：用 xitca-web 暴露引擎 API（访问交互层）。
//!
//! 监听 127.0.0.1:<port>，提供：
//! - GET  /api/sessions              会话列表
//! - POST /api/sessions              创建会话
//! - GET  /api/sessions/{id}         会话详情（含消息/事件）
//! - POST /api/sessions/{id}/send    发送消息（异步触发 agent 回合）
//! - GET  /api/settings              引擎设置（不含密钥）
//!
//! 鉴权：所有端点要求 `Authorization: Bearer <token>` 或 `X-DSH-Token: <token>`。
//! token 每次启动随机生成（无鉴权 + 默认 danger-full-access 的组合等于把
//! 任意命令执行开放给本机任意进程/网页——CORS 简单请求即可触发）。
//!
//! 访问交互层完全用 xitca-web（与 ada-rs 同款框架）实现。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use log::{info, warn};
use xitca_web::{
    handler::{handler_service, json::Json, params::Params, state::StateRef},
    http::WebRequest,
    route::{get, post},
    App,
};

use serde::Deserialize;
use serde_json::json;

use crate::core::{DshEngine, Session};

/// 路径参数。
#[derive(Deserialize)]
struct SessionParams {
    id: String,
}

/// 常量时间比较（本地回环下时序侧信道利用难度高，仍按最佳实践）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 共享引擎。
pub type SharedEngine = Arc<Mutex<DshEngine>>;

/// 桥接服务状态（handler 共享）。
struct BridgeState {
    pub engine: SharedEngine,
    /// 本次启动的随机鉴权 token
    token: String,
}

/// 在一个后台线程里启动 xitca-web 服务。
/// 返回 (实际绑定端口, 鉴权 token)。
pub fn start_bridge(engine: SharedEngine) -> Result<(u16, String)> {
    let port = find_free_port()?;
    let token = random_token();
    let state = Arc::new(BridgeState {
        engine,
        token: token.clone(),
    });

    let state_ref = state.clone();
    let _ = std::thread::Builder::new()
        .name("dsh-bridge".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("bridge tokio runtime");
            let _guard = rt.enter();
            let _ = rt.block_on(async move {
                let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
                info!("dsh bridge (xitca-web) listening on http://{addr}");
                let app = App::new()
                    .with_state(state_ref)
                    .at(
                        "/api/sessions",
                        get(handler_service(list_sessions)).post(handler_service(create_session)),
                    )
                    .at("/api/sessions/{id}", get(handler_service(get_session)))
                    .at(
                        "/api/sessions/{id}/send",
                        post(handler_service(send_message)),
                    )
                    .at("/api/settings", get(handler_service(get_settings)));
                match app.serve().bind(addr) {
                    Ok(server) => {
                        if let Err(e) = server.run().await {
                            warn!("bridge server error: {e}");
                        }
                    }
                    Err(e) => warn!("bridge bind error: {e}"),
                }
            });
        })
        .expect("spawn bridge thread");
    Ok((port, token))
}

/// 找个空闲端口。
fn find_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// 随机 token（无 rand 依赖：RandomState 的 SipHash 密钥由 OS RNG 播种）。
fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut s = String::new();
    for _ in 0..2 {
        s.push_str(&format!(
            "{:016x}",
            RandomState::new().build_hasher().finish()
        ));
    }
    s
}

/// 校验请求的 token（Authorization: Bearer <t> 或 X-DSH-Token: <t>）。
fn authorized(req: &WebRequest<()>, state: &BridgeState) -> bool {
    let supplied = req
        .headers()
        .get("x-dsh-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_owned)
        });
    match supplied {
        Some(t) => constant_time_eq(t.as_bytes(), state.token.as_bytes()),
        None => false,
    }
}

// ===== handlers =====

async fn list_sessions(
    StateRef(state): StateRef<'_, BridgeState>,
    req: &WebRequest<()>,
) -> Json<serde_json::Value> {
    if !authorized(req, &state) {
        return unauthorized();
    }
    let engine = state.engine.lock().unwrap_or_else(|p| p.into_inner());
    let sessions = engine.list_sessions();
    Json(serde_json::to_value(&sessions).unwrap_or_else(|_| serde_json::json!([])))
}

async fn create_session(
    StateRef(state): StateRef<'_, BridgeState>,
    req: &WebRequest<()>,
) -> Json<serde_json::Value> {
    if !authorized(req, &state) {
        return unauthorized();
    }
    let mut engine = state.engine.lock().unwrap_or_else(|p| p.into_inner());
    match engine.create_session(None) {
        Ok(id) => Json(serde_json::json!({"sessionId": id})),
        Err(e) => Json(serde_json::json!({"error": format!("{e:#}"), "code": 500})),
    }
}

async fn get_session(
    StateRef(state): StateRef<'_, BridgeState>,
    req: &WebRequest<()>,
    params: Result<Params<SessionParams>, xitca_web::error::Error>,
) -> Json<serde_json::Value> {
    if !authorized(req, &state) {
        return unauthorized();
    }
    let params = match params {
        Ok(p) => p.0,
        Err(_) => return Json(json!({"error": "missing id", "code": 400})),
    };
    let mut engine = state.engine.lock().unwrap_or_else(|p| p.into_inner());
    match engine.open_session(&params.id) {
        Ok(session) => Json(session_payload(&session)),
        Err(e) => Json(serde_json::json!({"error": format!("{e:#}"), "code": 404})),
    }
}

async fn send_message(
    StateRef(state): StateRef<'_, BridgeState>,
    req: &WebRequest<()>,
    params: Result<Params<SessionParams>, xitca_web::error::Error>,
    body: String,
) -> Json<serde_json::Value> {
    if !authorized(req, &state) {
        return unauthorized();
    }
    let params = match params {
        Ok(p) => p.0,
        Err(_) => return Json(json!({"error": "missing id", "code": 400})),
    };
    let content = match serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("content").and_then(|c| c.as_str()).map(String::from))
    {
        Some(c) => c,
        None => return Json(json!({"error": "missing content", "code": 400})),
    };
    // 日志不落消息内容（隐私：内容可能含代码/密钥）
    log::debug!(
        "bridge send_message id={} content_len={}",
        params.id,
        content.len()
    );
    let mut engine = state.engine.lock().unwrap_or_else(|p| p.into_inner());
    match engine.send_message(&params.id, &content) {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"error": format!("{e:#}"), "code": 400})),
    }
}

async fn get_settings(
    StateRef(state): StateRef<'_, BridgeState>,
    req: &WebRequest<()>,
) -> Json<serde_json::Value> {
    if !authorized(req, &state) {
        return unauthorized();
    }
    let engine = state.engine.lock().unwrap_or_else(|p| p.into_inner());
    let s = engine.settings();
    Json(serde_json::json!({
        "model": s.model,
        "baseUrl": s.base_url,
        "reasoningEffort": s.reasoning_effort,
        "hasApiKey": s.api_key.is_some(),
        "proxy": s.http_proxy,
        "dataDir": s.data_dir,
    }))
}

fn unauthorized() -> Json<serde_json::Value> {
    Json(json!({
        "error": "unauthorized: missing or invalid token (Authorization: Bearer <token> / X-DSH-Token)",
        "code": 401,
    }))
}

// ===== helpers =====

fn session_payload(s: &Session) -> serde_json::Value {
    serde_json::json!({
        "sessionId": s.id,
        "title": s.title,
        "messages": s.messages,
        "running": s.running,
        "eventCount": s.events.len(),
    })
}
