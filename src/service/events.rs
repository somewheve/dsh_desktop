//! 事件流推送：SSE / WS（对齐 dsh-host-apiproxy 的 events.mux / events.host 语义）。
//!
//! dsh-desktop 的 UI 直连引擎，事件经 mpsc 通道实时到达；
//! 本模块为外部客户端（桥）提供 SSE 推送格式。

use crate::core::SessionEvent;

/// SSE 帧编码（data: <json>\n\n）。
pub fn sse_frame(ev: &SessionEvent) -> String {
    let payload = serde_json::to_string(ev).unwrap_or_else(|_| "{}".into());
    format!("data: {payload}\n\n")
}

/// 事件流聚合：把多个事件编码为 SSE 文本块。
pub fn sse_batch(events: &[SessionEvent]) -> String {
    events.iter().map(sse_frame).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::types;

    #[test]
    fn sse_frame_shape() {
        let ev = SessionEvent::new(types::TURN_START, None);
        let frame = sse_frame(&ev);
        assert!(frame.starts_with("data: "));
        assert!(frame.ends_with("\n\n"));
        assert!(frame.contains("\"type\":\"turn/start\""));
    }

    #[test]
    fn sse_batch_joins() {
        let evs = vec![
            SessionEvent::new(types::TURN_START, None),
            SessionEvent::new(types::TURN_END, None),
        ];
        let batch = sse_batch(&evs);
        assert_eq!(batch.matches("data: ").count(), 2);
    }
}
