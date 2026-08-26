//! 插件协议：JSON-RPC 2.0（stdin/stdout 行分隔），对齐 cordis 语义。
//!
//! 消息流：
//! - Rust → 插件：initialize（含工作目录/会话上下文）→ 插件回 result（可空）
//! - 插件 → Rust：`plugin.ready` notification（上报 tools/services 清单）
//! - Rust → 插件：`tool.invoke` request（id 匹配响应）；`service.call` request
//! - Rust → 插件：`event.emit` notification（会话/引擎事件转发）
//! - 插件 → Rust：`plugin.event` notification（插件主动发事件）

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 请求/通知。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl RpcRequest {
    pub fn call(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: Some("2.0".into()),
            id: Some(id),
            method: method.into(),
            params,
        }
    }

    pub fn notify(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: Some("2.0".into()),
            id: None,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// 来自插件的一行 JSON（可能是 response 或 notification）。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RpcIn {
    Response(RpcResponse),
    Notification(RpcRequest),
}

/// 工具规范（与 agent 工具同构：name/description/parameters）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_params")]
    pub parameters: Value,
}

fn default_params() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// plugin.ready notification 的 params。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginReady {
    #[serde(default)]
    pub tools: Vec<PluginToolSpec>,
    #[serde(default)]
    pub services: Vec<String>,
}
