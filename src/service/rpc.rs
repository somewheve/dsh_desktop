//! Typert RPC 信封：对齐 dsh-typert-protocol + dsh-api-gateway 语义。
//!
//! 客户端请求：{ rpcId, namespace, method, args }
//! 服务端响应：{ rpcId, result: { ok: true, value } | { ok: false, error: { code, message } } }

use serde::{Deserialize, Serialize};

/// 段名文法（对齐 TYPERT_REMOTE_SEGMENT_PATTERN）。
pub fn is_remote_segment(value: &str) -> bool {
    value != "."
        && value != ".."
        && !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.' || c == '-')
}

/// 客户端 RPC 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub rpc_id: String,
    pub namespace: String,
    pub method: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// RPC 错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

/// RPC 结果（ok / error 二分支，对齐 DSH {ok, value/error} 形状）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcResult {
    Ok { ok: bool, value: serde_json::Value },
    Err { ok: bool, error: RpcError },
}

/// RPC 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub rpc_id: String,
    pub result: RpcResult,
}

impl RpcRequest {
    /// 端点：namespace/method（对齐 endpointOf）。
    pub fn endpoint(&self) -> String {
        format!("{}/{}", self.namespace, self.method)
    }

    /// 校验信封。
    pub fn validate(&self) -> Result<(), String> {
        if !is_remote_segment(&self.namespace) {
            return Err(format!("invalid namespace segment: {:?}", self.namespace));
        }
        if !is_remote_segment(&self.method) {
            return Err(format!("invalid method segment: {:?}", self.method));
        }
        Ok(())
    }
}

impl RpcResponse {
    pub fn ok(rpc_id: &str, value: serde_json::Value) -> Self {
        Self {
            rpc_id: rpc_id.to_string(),
            result: RpcResult::Ok { ok: true, value },
        }
    }
    pub fn err(rpc_id: &str, code: &str, message: &str) -> Self {
        Self {
            rpc_id: rpc_id.to_string(),
            result: RpcResult::Err {
                ok: false,
                error: RpcError {
                    code: code.to_string(),
                    message: message.to_string(),
                },
            },
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(self.result, RpcResult::Ok { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_grammar() {
        assert!(is_remote_segment("session.list"));
        assert!(is_remote_segment("pluginInventory"));
        assert!(!is_remote_segment("."));
        assert!(!is_remote_segment(".."));
        assert!(!is_remote_segment("has space"));
    }

    #[test]
    fn envelope_roundtrip() {
        let req = RpcRequest {
            rpc_id: "r1".into(),
            namespace: "session".into(),
            method: "list".into(),
            args: serde_json::json!({}),
        };
        assert_eq!(req.endpoint(), "session/list");
        assert!(req.validate().is_ok());
        let json = serde_json::to_string(&req).unwrap();
        let back: RpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rpc_id, "r1");
    }

    #[test]
    fn ok_err_shapes() {
        let ok = RpcResponse::ok("r1", serde_json::json!({"items": []}));
        assert!(ok.is_ok());
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains("\"ok\":true"));
        let err = RpcResponse::err("r1", "not-found", "missing");
        assert!(!err.is_ok());
    }
}
