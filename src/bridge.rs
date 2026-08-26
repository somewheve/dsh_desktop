//! 本地桥接服务：用 xitca-web 暴露引擎 API（访问交互层）。
//!
//! 监听 127.0.0.1:<port>，提供：
//! - GET  /api/sessions              会话列表
//! - POST /api/sessions              创建会话
//! - GET  /api/sessions/{id}         会话详情（含消息/事件）
//! - POST /api/sessions/{id}/send    发送消息（异步触发 agent 回合）
//! - GET  /api/settings              引擎设置（不含密钥）
//!
//! 访问交互层完全用 xitca-web（与 ada-rs 同款框架）实现。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use log::{info, warn};
use xitca_web::{
    handler::{handler_service, json::Json, params::Params, state::StateRef},
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

/// 共享引擎。
pub type SharedEngine = Arc<Mutex<DshEngine>>;

/// 桥接服务状态（handler 共享）。
pub struct BridgeState {
    pub engine: SharedEngine,
}

/// 在一个后台线程里启动 xitca-web 服务。
/// 返回实际绑定端口。
pub fn start_bridge(engine: SharedEngine) -> Result<u16> {
    let port = find_free_port()?;
    let state = Arc::new(BridgeState { engine });

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
    Ok(port)
}

/// 找个空闲端口。
fn find_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

// ===== handlers =====

async fn list_sessions(StateRef(state): StateRef<'_, BridgeState>) -> Json<serde_json::Value> {
    let engine = state.engine.lock().unwrap();
    let sessions = engine.list_sessions();
    Json(serde_json::to_value(&sessions).unwrap_or_else(|_| serde_json::json!([])))
}

async fn create_session(StateRef(state): StateRef<'_, BridgeState>) -> Json<serde_json::Value> {
    let mut engine = state.engine.lock().unwrap();
    match engine.create_session(None) {
        Ok(id) => Json(serde_json::json!({"sessionId": id})),
        Err(e) => Json(serde_json::json!({"error": format!("{e:#}"), "code": 500})),
    }
}

async fn get_session(
    StateRef(state): StateRef<'_, BridgeState>,
    params: Result<Params<SessionParams>, xitca_web::error::Error>,
) -> Json<serde_json::Value> {
    let params = match params {
        Ok(p) => p.0,
        Err(_) => return Json(json!({"error": "missing id", "code": 400})),
    };
    let mut engine = state.engine.lock().unwrap();
    match engine.open_session(&params.id) {
        Ok(session) => Json(session_payload(&session)),
        Err(e) => Json(json!({"error": format!("{e:#}"), "code": 404})),
    }
}

async fn send_message(
    StateRef(state): StateRef<'_, BridgeState>,
    params: Result<Params<SessionParams>, xitca_web::error::Error>,
    body: String,
) -> Json<serde_json::Value> {
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
    log::debug!("bridge send_message id={} content={}", params.id, content);
    let mut engine = state.engine.lock().unwrap();
    match engine.send_message(&params.id, &content) {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"error": format!("{e:#}"), "code": 400})),
    }
}

async fn get_settings(StateRef(state): StateRef<'_, BridgeState>) -> Json<serde_json::Value> {
    let engine = state.engine.lock().unwrap();
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
