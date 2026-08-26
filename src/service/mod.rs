//! 服务层：RPC 信封 + 事件流（对齐 dsh-typert-protocol / dsh-api-gateway / dsh-host-apiproxy）。

pub mod events;
pub mod rpc;

pub use events::{sse_batch, sse_frame};
pub use rpc::{is_remote_segment, RpcError, RpcRequest, RpcResponse, RpcResult};
