//! LLM 客户端：DeepSeek 官方 API（OpenAI 兼容、流式 SSE）。
//!
//! 对齐 DSH 的 dsh-llm（OpenAI 兼容协议、流式响应、retry-policy 语义），
//! 用 Rust 原生重写。支持：chat.completions 流式、工具调用、reasoning。

use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::session::{FunctionCall, Message, ToolCall};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: LlmRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub spec_type: String,
    pub function: FunctionSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// DeepSeek API 模型表（2026-08 官方价目页；legacy 名称保留兼容旧配置）。
pub const DEEPSEEK_MODELS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "deepseek-v4-flash-vision-exp",
];

/// 思考深度档位（对齐官方 reasoning_effort 值域；none = 关闭思考）。
pub const REASONING_EFFORTS: &[&str] = &["none", "low", "high", "max"];

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<LlmMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// 思考模式开关（V4 官方参数 thinking.type = enabled/disabled；
    /// effort=none → disabled，其余 enabled）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

/// `{"thinking": {"type": "enabled" | "disabled"}}`。
#[derive(Debug, Clone, Serialize)]
pub struct ThinkingConfig {
    pub r#type: String,
}

/// 流式 chunk（OpenAI SSE data 行）。
#[derive(Debug, Clone, Deserialize)]
pub struct StreamChunk {
    pub choices: Vec<StreamChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamChoice {
    pub delta: Delta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FunctionCallDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// 一次流式调用的回调事件。
pub enum StreamEvent {
    /// 文本增量
    Chunk(String),
    /// reasoning 增量（deepseek-reasoner）
    Reasoning(String),
    /// 工具调用增量（累积后由调用方组装）
    ToolCallAccum {
        index: usize,
        id: String,
        name: String,
        arguments: String,
    },
    /// 流结束
    Done,
    /// 错误
    Error(String),
}

pub struct LlmClient {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    reasoning_effort: Option<String>,
}

/// 单次流式请求的结果（stream 内部用）。
enum StreamOutcome {
    Done,
    Failed {
        err: anyhow::Error,
        /// 是否可安全重试（未发出任何流事件 + 瞬态故障）
        retryable: bool,
    },
}

impl LlmClient {
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        reasoning_effort: Option<String>,
        proxy: Option<String>,
    ) -> Result<Self> {
        // 超时策略：连接超时 + 读空闲超时（两次数据之间），**不用总超时**——
        // 总超时是"从发请求到流读完"的死线，长 reasoning 生成会被腰斩，
        // 已流式输出的文本随回合失败全部丢失。
        let mut builder = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(180));
        if let Some(p) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(&p).context("invalid proxy")?);
        }
        let client = builder.build().context("build http client")?;
        Ok(Self {
            client,
            base_url,
            api_key,
            model,
            reasoning_effort,
        })
    }

    /// 流式 chat.completions。
    ///
    /// 连接失败 / 429 / 5xx 自动重试（指数退避，最多 3 次）——重试只发生在
    /// **任何流事件发出之前**（幂等），半途断流不重试（避免 UI 内容重复）。
    pub async fn stream(
        &self,
        messages: &[LlmMessage],
        tools: Option<&[ToolSpec]>,
        on_event: impl FnMut(StreamEvent) + Send + 'static,
    ) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let req = ChatRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
            stream: true,
            tools: tools.map(|t| t.to_vec()),
            // none 档不发 effort（发 thinking disabled 关闭思考）
            reasoning_effort: match self.reasoning_effort.as_deref() {
                Some("none") | None => None,
                Some(e) => Some(e.to_string()),
            },
            thinking: Some(ThinkingConfig {
                r#type: match self.reasoning_effort.as_deref() {
                    Some("none") => "disabled".into(),
                    _ => "enabled".into(),
                },
            }),
        };
        let mut on_event = on_event;
        const MAX_ATTEMPTS: u32 = 3;
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self.stream_once(&url, &req, &mut on_event).await {
                StreamOutcome::Done => return Ok(()),
                StreamOutcome::Failed { err, retryable } => {
                    let retryable = retryable && attempt + 1 < MAX_ATTEMPTS;
                    if !retryable {
                        return Err(err);
                    }
                    // 瞬态故障：陈旧 keep-alive 连接、限流、网关抖动。
                    // 退避取短档（连接拒绝等确定性失败也要快速确认后失败，
                    // 不能让用户等秒级重试）
                    let backoff_ms = [400u64, 800][attempt as usize];
                    log::warn!(
                        "llm attempt {}/{} failed (will retry in {backoff_ms}ms): {err:#}",
                        attempt + 1,
                        MAX_ATTEMPTS
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("llm stream failed")))
    }

    /// 单次流式请求。任何流事件（Chunk/Reasoning/ToolCallAccum）发出后
    /// 再失败即不可重试。
    async fn stream_once(
        &self,
        url: &str,
        req: &ChatRequest,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> StreamOutcome {
        let resp = match self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(req)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // 连接层失败：仅**超时**重试（服务器慢/网络抖动）。
                // 连接拒绝是确定性失败（Windows loopback 拒绝一次 ~2s 的
                // SYN 重试，再乘重试次数会让用户干等），立即失败。
                let retryable = e.is_timeout();
                return StreamOutcome::Failed {
                    err: anyhow::anyhow!("llm request failed ({url}): {e}"),
                    retryable,
                };
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let retryable = status.as_u16() == 429 || status.is_server_error();
            return StreamOutcome::Failed {
                err: anyhow::anyhow!("llm error {status}: {body}"),
                retryable,
            };
        }

        let mut tool_acc: std::collections::HashMap<usize, (String, String, String)> =
            Default::default();
        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        // 是否已向调用方发过流事件（发过 → 断流不可重试，重试会内容重复）
        let mut emitted = false;
        let mut had_payload = false; // 收到过任何 delta（content/reasoning/tool）
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let partial = emitted;
                    return StreamOutcome::Failed {
                        err: anyhow::anyhow!(
                            "llm stream read failed{}: {e}",
                            if partial {
                                "（已有部分输出）"
                            } else {
                                ""
                            }
                        ),
                        retryable: false,
                    };
                }
            };
            buf.extend_from_slice(&chunk);
            // 按行解析 SSE（data: {...}）
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let data = line.strip_prefix("data:").map(|s| s.trim()).unwrap_or(line);
                if data == "[DONE]" {
                    // 收尾：把累积的工具调用发出去（此处 emitted 已无后续读点）
                    for (idx, (id, name, args)) in tool_acc.drain() {
                        on_event(StreamEvent::ToolCallAccum {
                            index: idx,
                            id,
                            name,
                            arguments: args,
                        });
                    }
                    on_event(StreamEvent::Done);
                    return StreamOutcome::Done;
                }
                match serde_json::from_str::<StreamChunk>(data) {
                    Ok(parsed) => {
                        for choice in parsed.choices {
                            if let Some(c) = choice.delta.content {
                                if !c.is_empty() {
                                    had_payload = true;
                                    emitted = true;
                                    on_event(StreamEvent::Chunk(c));
                                }
                            }
                            if let Some(r) = choice.delta.reasoning_content {
                                if !r.is_empty() {
                                    had_payload = true;
                                    emitted = true;
                                    on_event(StreamEvent::Reasoning(r));
                                }
                            }
                            if let Some(tcs) = choice.delta.tool_calls {
                                had_payload = true;
                                emitted = true;
                                for tc in tcs {
                                    let entry = tool_acc.entry(tc.index).or_insert_with(|| {
                                        (String::new(), String::new(), String::new())
                                    });
                                    if let Some(id) = tc.id {
                                        entry.0 = id;
                                    }
                                    if let Some(f) = tc.function {
                                        if let Some(name) = f.name {
                                            entry.1 = name;
                                        }
                                        if let Some(args) = f.arguments {
                                            entry.2.push_str(&args);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // 非 JSON 行（如空行、注释），忽略
                    }
                }
            }
        }
        // 连接结束但没有 [DONE]：截断——无论缓冲区/工具累积是否为空，
        // 服务端或代理在生成中途断开都会走到这里；把半截内容当成功会让
        // 被腰斩的回复静默进入历史（旧实现只在缓冲非空时报错，纯文本流
        // 的截断会漏检）。
        let hint = if had_payload {
            "（已有部分输出被丢弃/降级保留）"
        } else {
            "（未收到任何内容）"
        };
        on_event(StreamEvent::Error(format!(
            "LLM 流提前结束（未收到 [DONE]）{hint}"
        )));
        StreamOutcome::Failed {
            err: anyhow::anyhow!(
                "llm stream ended without [DONE] ({} bytes buffered){hint}",
                buf.len()
            ),
            retryable: false,
        }
    }

    /// 从 session Message 转换 LLM 消息（带防御性配对修复）。
    ///
    /// OpenAI 兼容约束：assistant 的 tool_calls 后必须有响应每个 tool_call_id
    /// 的 tool 消息；tool 消息前必须有携带 tool_calls 的 assistant 消息。
    /// 若序列因中断/重放缺配而损坏，这里裁剪未响应的 tool_calls 并丢弃孤立
    /// tool 消息，宁可少调用工具也不发非法请求。
    pub fn to_llm_messages(messages: &[Message]) -> Vec<LlmMessage> {
        let mut out: Vec<LlmMessage> = Vec::new();
        // 延迟提交模式：assistant 的 tool_calls 先全量进 out，
        // 在遇到下一条 user/assistant 或结尾时才裁剪为"已响应子集"，
        // 并同步丢弃因此失去配对的孤儿 tool 消息，避免生成非法序列。
        let mut pending: Vec<String> = Vec::new(); // 当前 assistant 声明的调用 id
        let mut responded: Vec<String> = Vec::new(); // 其中已收到响应的
        let mut assistant_idx: Option<usize> = None;
        for m in messages {
            match m {
                Message::Assistant {
                    content,
                    tool_calls,
                } => {
                    trim_pending(&mut out, &mut pending, &mut responded, &mut assistant_idx);
                    pending = tool_calls.iter().map(|tc| tc.id.clone()).collect();
                    responded.clear();
                    assistant_idx = Some(out.len());
                    out.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: if content.is_empty() && !tool_calls.is_empty() {
                            None
                        } else {
                            Some(content.clone())
                        },
                        tool_call_id: None,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls.clone())
                        },
                        name: None,
                    });
                }
                Message::Tool {
                    tool_call_id,
                    content,
                } => {
                    if pending.contains(tool_call_id) {
                        if !responded.contains(tool_call_id) {
                            responded.push(tool_call_id.clone());
                        }
                        out.push(LlmMessage {
                            role: LlmRole::Tool,
                            content: Some(content.clone()),
                            tool_call_id: Some(tool_call_id.clone()),
                            tool_calls: None,
                            name: None,
                        });
                    } else {
                        // 孤立 tool 消息（前无 assistant tool_calls）→ 丢弃，避免 400
                        log::warn!("丢弃孤立 tool 消息（{tool_call_id}）");
                    }
                }
                Message::User { content } => {
                    trim_pending(&mut out, &mut pending, &mut responded, &mut assistant_idx);
                    out.push(LlmMessage {
                        role: LlmRole::User,
                        content: Some(content.clone()),
                        tool_call_id: None,
                        tool_calls: None,
                        name: None,
                    });
                }
            }
        }
        trim_pending(&mut out, &mut pending, &mut responded, &mut assistant_idx);
        // 防御：中断/停止/插话后，assistant 的 tool_calls 可能被裁剪为空，
        // 且原本就没有 content → 产生既无 content 又无 tool_calls 的 assistant
        // 空壳。OpenAI 兼容 API 对此报 400 (content or tool_calls must be set)。
        // 这类消息没有任何信息量，直接移除（不影响上下文）。
        out.retain(|m| {
            if m.role == LlmRole::Assistant && m.content.is_none() && m.tool_calls.is_none() {
                log::warn!("移除空壳 assistant 消息（无 content 且无 tool_calls）");
                false
            } else {
                true
            }
        });
        out
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// 把当前 assistant 的 tool_calls 裁剪为已响应子集；未响应的调用移除，
/// 若全部未响应则整条置 None（对应 tool 消息已在响应时入 out，保持配对）。
fn trim_pending(
    out: &mut Vec<LlmMessage>,
    pending: &mut Vec<String>,
    responded: &mut Vec<String>,
    assistant_idx: &mut Option<usize>,
) {
    if let Some(idx) = *assistant_idx {
        if let Some(assistant) = out.get_mut(idx) {
            if assistant.role == LlmRole::Assistant {
                if let Some(tcs) = assistant.tool_calls.take() {
                    let keep: Vec<_> = tcs
                        .into_iter()
                        .filter(|tc| responded.contains(&tc.id))
                        .collect();
                    if !keep.is_empty() {
                        assistant.tool_calls = Some(keep);
                    }
                }
            }
        }
    }
    // 仅真正发生裁剪（有未响应的调用）时才告警；正常全量配对不打扰
    let unmatched = pending.len().saturating_sub(responded.len());
    if unmatched > 0 {
        log::warn!(
            "裁剪未响应的 tool_calls：声明 {} 个，响应 {} 个",
            pending.len(),
            responded.len()
        );
    }
    pending.clear();
    responded.clear();
    *assistant_idx = None;
}

/// 构造 ToolSpec 辅助。
pub fn tool_spec(name: &str, description: &str, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        spec_type: "function".into(),
        function: FunctionSpec {
            name: name.into(),
            description: description.into(),
            parameters,
        },
    }
}

/// 工具调用累积结果 → 完整 ToolCall。
pub fn finish_tool_call(_index: usize, id: String, name: String, arguments: String) -> ToolCall {
    ToolCall {
        id,
        call_type: "function".into(),
        function: FunctionCall { name, arguments },
    }
}

/// Arc 包装便于在线程间共享。
pub type SharedLlm = Arc<LlmClient>;

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "ask_user".into(),
                arguments: "{}".into(),
            },
        }
    }

    /// 回归：assistant 仅声明 tool_calls（无文本 content）且没有对应 tool 响应
    /// （ask_user 暂停 / 中断 / 停止路径）时，不得产生"无 content 且无
    /// tool_calls"的 assistant 空壳 —— OpenAI 兼容 API 对它会 400
    /// (content or tool_calls must be set)。空壳消息应被移除。
    #[test]
    fn no_empty_shell_assistant_after_trim() {
        let msgs = vec![
            Message::User {
                content: "继续".into(),
            },
            Message::Assistant {
                content: String::new(), // 纯工具调用回合：无文本
                tool_calls: vec![tc("call-1")],
            },
        ];
        let out = LlmClient::to_llm_messages(&msgs);
        for m in &out {
            if m.role == LlmRole::Assistant {
                assert!(
                    m.content.is_some() || m.tool_calls.is_some(),
                    "assistant 消息不能既无 content 又无 tool_calls（空壳 → 400）: {m:?}"
                );
            }
        }
        // 空壳被移除后只剩 user
        assert_eq!(out.len(), 1, "空壳 assistant 应被移除: {out:?}");
    }

    /// 正常配对（assistant tool_calls + tool 响应）必须完整保留。
    #[test]
    fn paired_tool_calls_kept() {
        let msgs = vec![
            Message::User {
                content: "hi".into(),
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![tc("call-1")],
            },
            Message::Tool {
                tool_call_id: "call-1".into(),
                content: "ok".into(),
            },
        ];
        let out = LlmClient::to_llm_messages(&msgs);
        assert_eq!(out.len(), 3);
        let a = &out[1];
        assert!(a.tool_calls.is_some(), "配对完整时 tool_calls 必须保留");
        assert!(a.content.is_none() || a.content.as_deref() == Some(""));
    }

    /// 部分响应（中断）：未响应的 tool_calls 被裁剪，但保留已响应的，不产生空壳。
    #[test]
    fn partial_response_trims_unmatched_only() {
        let msgs = vec![
            Message::User {
                content: "hi".into(),
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![tc("call-1"), tc("call-2")],
            },
            Message::Tool {
                tool_call_id: "call-1".into(),
                content: "ok".into(),
            },
        ];
        let out = LlmClient::to_llm_messages(&msgs);
        let a = out.iter().find(|m| m.role == LlmRole::Assistant).unwrap();
        let kept: Vec<_> = a
            .tool_calls
            .as_ref()
            .unwrap()
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(kept, vec!["call-1"], "未响应的 call-2 应被裁剪: {kept:?}");
    }

    /// 回归：ask_user 工具结果必须作为 tool 响应入消息序列（run_turn 侧），
    /// 此处验证配对序列转换后 assistant 保留 tool_calls（不会 400）。
    #[test]
    fn ask_user_result_paired_keeps_tool_calls() {
        let msgs = vec![
            Message::User {
                content: "跑一下".into(),
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![tc("call-ask")],
            },
            Message::Tool {
                tool_call_id: "call-ask".into(),
                content: "{\"await_user\":true,\"question\":\"继续？\"}".into(),
            },
        ];
        let out = LlmClient::to_llm_messages(&msgs);
        let a = out.iter().find(|m| m.role == LlmRole::Assistant).unwrap();
        assert!(
            a.tool_calls.is_some(),
            "ask_user 已配对时 tool_calls 必须保留"
        );
    }
}
