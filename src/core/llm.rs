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

#[derive(Debug, Clone, Deserialize)]
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
    /// 多模态图片（data:image/...;base64,...）。有图时 content 在序列化时
    /// 自动变为 OpenAI 视觉格式的分段数组：
    /// `[{type:text},{type:image_url}]`；无图保持纯字符串（兼容旧行为）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
}

impl LlmMessage {
    /// 构造（无图，兼容旧调用点）。
    pub fn text(role: LlmRole, content: Option<String>) -> Self {
        Self {
            role,
            content,
            tool_call_id: None,
            tool_calls: None,
            name: None,
            images: None,
        }
    }
}

/// 多模态：有 images 时 content 序列化为分段数组。
/// 手动实现（字段形状随 images 切换，无法用 derive 表达）。
impl Serialize for LlmMessage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_multimodal(self, s)
    }
}

fn serialize_multimodal<S: serde::Serializer>(
    msg: &LlmMessage,
    s: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut map = s.serialize_map(None)?;
    map.serialize_entry("role", &msg.role)?;
    match (&msg.content, &msg.images) {
        (Some(text), Some(uris)) if !uris.is_empty() => {
            let mut parts: Vec<serde_json::Value> = Vec::with_capacity(uris.len() + 1);
            if !text.is_empty() {
                parts.push(serde_json::json!({"type": "text", "text": text}));
            }
            for uri in uris {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": uri}
                }));
            }
            map.serialize_entry("content", &parts)?;
        }
        _ => {
            if let Some(text) = &msg.content {
                map.serialize_entry("content", text)?;
            }
        }
    }
    if let Some(id) = &msg.tool_call_id {
        map.serialize_entry("tool_call_id", id)?;
    }
    if let Some(calls) = &msg.tool_calls {
        map.serialize_entry("tool_calls", calls)?;
    }
    if let Some(name) = &msg.name {
        map.serialize_entry("name", name)?;
    }
    map.end()
}

/// 图片扩展名 → data URI MIME。
fn image_mime(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// 单图大小护栏（data URI 前的原始字节）。
const IMAGE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 读取图片文件 → data URI（超限/不可读/非图片扩展返回 None）。
pub fn image_to_data_uri(path: &str) -> Option<String> {
    let mime = image_mime(path)?;
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > IMAGE_MAX_BYTES {
        log::warn!("image too large, skipped: {path} ({} bytes)", meta.len());
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{mime};base64,{b64}"))
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

/// DeepSeek API 模型表。已对 dsh 0.1.5 全栈核对
/// （dsh-llm-deepseek / dsh-client-ui-settings-models /
/// dsh-agent-default-model）：v4-flash、v4-pro、v4-flash-vision-exp
/// 即当前全部在售模型。
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
    /// 流式用量统计（include_usage：最后一个 chunk 携带 usage）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

/// `{"include_usage": true}`。
#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// 一次调用的 token 用量（API usage 字段）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.prompt_tokens += rhs.prompt_tokens;
        self.completion_tokens += rhs.completion_tokens;
    }
}

/// 模型上下文窗口（token，估算用；未知模型取保守 64k）。
pub fn context_window(model: &str) -> u64 {
    if model.contains("v4") || model.contains("reasoner") {
        128_000
    } else {
        64_000
    }
}

/// `{"thinking": {"type": "enabled" | "disabled"}}`。
#[derive(Debug, Clone, Serialize)]
pub struct ThinkingConfig {
    pub r#type: String,
}

/// 流式 chunk（OpenAI SSE data 行）。
#[derive(Debug, Clone, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    /// include_usage 时最后一个 chunk 携带（此时 choices 为空数组）
    #[serde(default)]
    pub usage: Option<UsageChunk>,
}

/// usage 对象（缺字段按 0 处理）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct UsageChunk {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
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
    /// token 用量（include_usage 的末 chunk）
    Usage(TokenUsage),
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

    /// 同配置换模型的轻量变体（子代理分级：fork 是 token 放大器，
    /// 机械子任务可走更便宜的档位）。reqwest Client 内部是 Arc，克隆廉价。
    pub fn with_model(&self, model: &str) -> Self {
        Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: model.to_string(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
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
            // 请求用量统计：末 chunk 返回 usage（UI 显示 token 消耗）
            stream_options: Some(StreamOptions { include_usage: true }),
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
                        // 用量 chunk（include_usage：末尾携带，choices 为空）
                        if let Some(u) = parsed.usage {
                            on_event(StreamEvent::Usage(TokenUsage {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                            }));
                        }
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
                    ..
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
                        images: None,
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
                            images: None,
                        });
                    } else {
                        // 孤立 tool 消息（前无 assistant tool_calls）→ 丢弃，避免 400
                        log::warn!("丢弃孤立 tool 消息（{tool_call_id}）");
                    }
                }
                Message::User { content, images } => {
                    trim_pending(&mut out, &mut pending, &mut responded, &mut assistant_idx);
                    // 图片路径 → data URI（读盘 + base64；不可读/超限跳过并注记）
                    let uris: Option<Vec<String>> = images.as_ref().map(|paths| {
                        paths
                            .iter()
                            .filter_map(|p| match image_to_data_uri(p) {
                                Some(uri) => Some(uri),
                                None => {
                                    log::warn!("图片不可读或超限，已跳过: {p}");
                                    None
                                }
                            })
                            .collect()
                    });
                    let uris = match uris {
                        Some(u) if !u.is_empty() => Some(u),
                        _ => None,
                    };
                    out.push(LlmMessage {
                        role: LlmRole::User,
                        content: Some(content.clone()),
                        tool_call_id: None,
                        tool_calls: None,
                        name: None,
                        images: uris,
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

    /// 分级模型变体：换模型不改原客户端配置。
    #[test]
    fn with_model_keeps_config() {
        let c = LlmClient::new(
            "https://example.invalid".into(),
            "k".into(),
            "deepseek-v4-pro".into(),
            None,
            None,
        )
        .unwrap();
        let sub = c.with_model("deepseek-v4-flash");
        assert_eq!(sub.model(), "deepseek-v4-flash");
        assert_eq!(c.model(), "deepseek-v4-pro");
    }

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
                images: None,
            },
            Message::Assistant {
                content: String::new(), // 纯工具调用回合：无文本
                tool_calls: vec![tc("call-1")],
                reasoning: None,
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
                images: None,
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![tc("call-1")],
                reasoning: None,
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
                images: None,
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![tc("call-1"), tc("call-2")],
                reasoning: None,
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
                images: None,
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![tc("call-ask")],
                reasoning: None,
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

    /// usage chunk 解析：include_usage 的末 chunk（choices 空 + usage 字段）
    /// 与普通 chunk（无 usage）都必须可反序列化。
    #[test]
    fn usage_chunk_parses() {
        let normal: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
        )
        .unwrap();
        assert!(normal.usage.is_none());
        assert_eq!(normal.choices.len(), 1);

        let usage_only: StreamChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":1234,"completion_tokens":56}}"#,
        )
        .unwrap();
        let u = usage_only.usage.expect("usage chunk");
        assert_eq!(u.prompt_tokens, 1234);
        assert_eq!(u.completion_tokens, 56);

        // 未知扩展字段（prompt_cache_hit_tokens 等）不阻断解析
        let extended: StreamChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"prompt_cache_hit_tokens":9}}"#,
        )
        .unwrap();
        assert_eq!(extended.usage.unwrap().prompt_tokens, 1);
    }

    /// 多模态序列化：有 images 时 content 变分段数组，无图保持纯字符串。
    #[test]
    fn multimodal_message_serialization() {
        let with_img = LlmMessage {
            role: LlmRole::User,
            content: Some("看这张图".into()),
            tool_call_id: None,
            tool_calls: None,
            name: None,
            images: Some(vec!["data:image/png;base64,QUJD".into()]),
        };
        let v = serde_json::to_value(&with_img).unwrap();
        // content 是数组：[text, image_url]
        let parts = v.get("content").unwrap().as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "看这张图");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
        // images 字段本身不出现在线上格式（已内联进 content）
        assert!(v.get("images").is_none());

        // 无图：content 纯字符串（兼容旧请求格式）
        let plain = LlmMessage {
            role: LlmRole::User,
            content: Some("普通消息".into()),
            tool_call_id: None,
            tool_calls: None,
            name: None,
            images: None,
        };
        let v = serde_json::to_value(&plain).unwrap();
        assert_eq!(v["content"], "普通消息");

        // to_llm_messages：User.images 路径 → data URI（临时图片文件）
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("pic.png");
        std::fs::write(&img, b"fakepng").unwrap();
        let msgs = vec![Message::User {
            content: "描述".into(),
            images: Some(vec![img.display().to_string()]),
        }];
        let out = LlmClient::to_llm_messages(&msgs);
        assert_eq!(out.len(), 1);
        let uris = out[0].images.as_ref().unwrap();
        assert!(uris[0].starts_with("data:image/png;base64,"), "{}", uris[0]);

        // 不可读路径：跳过（images 归 None，不产生非法请求）
        let msgs = vec![Message::User {
            content: "x".into(),
            images: Some(vec!["Z:/no/such/img.png".into()]),
        }];
        let out = LlmClient::to_llm_messages(&msgs);
        assert!(out[0].images.is_none(), "不可读图片应跳过");
    }

    /// 用量累加与上下文窗口表。
    #[test]
    fn token_usage_math_and_context_window() {
        let mut a = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
        };
        a += TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 2,
        };
        assert_eq!(a.total(), 18);
        assert_eq!(context_window("deepseek-v4-flash"), 128_000);
        assert_eq!(context_window("deepseek-reasoner"), 128_000);
        assert_eq!(context_window("deepseek-chat"), 64_000);
        assert_eq!(context_window("unknown-model"), 64_000);
    }
}
