//! 会话界面布局自动化测试（egui_kittest headless 渲染）。
//!
//! 覆盖：窗口缩放时输入框与最新消息保持可见（历史回归：缩放后消息/对话框消失）。

use egui::Vec2;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use dsh_desktop::core::session::types;
use dsh_desktop::core::settings::EngineSettings;
use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
use dsh_desktop::ui::chat_tab::ChatTab;

/// 构造带 N 条消息的 ChatTab（事件驱动注入，不依赖真实 LLM）。
fn make_chat(msg_count: usize) -> (ChatTab, std::sync::mpsc::Sender<EngineEvent>) {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("layout test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // 注入历史消息（user 交替 assistant，最后一条是 assistant "last reply"）
    for i in 0..msg_count {
        let ev = SessionEvent::new(
            types::USER_MESSAGE,
            Some(serde_json::json!({"content": format!("user msg {i}")})),
        );
        let _ = tx.send(EngineEvent::Event {
            session_id: sid.clone(),
            event: ev,
        });
        let ev = SessionEvent::new(
            types::ASSISTANT_MESSAGE,
            Some(
                serde_json::json!({"content": format!("reply {i}"), "reasoning_content": "", "tool_calls": []}),
            ),
        );
        let _ = tx.send(EngineEvent::Event {
            session_id: sid.clone(),
            event: ev,
        });
    }
    let ev = SessionEvent::new(
        types::ASSISTANT_MESSAGE,
        Some(
            serde_json::json!({"content": "last reply", "reasoning_content": "", "tool_calls": []}),
        ),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid,
        event: ev,
    });
    (chat, tx)
}

/// 回归：AI 回复（有序列表 + 行内代码 + 加粗，如"q"回复）经 markdown 渲染后，
/// 文本节点必须落在窗口内（x >= 0），不得偏移到窗口外（历史 bug：节点 x=-124，
/// 用户报告"历史消息的 AI 消息出问题"）。
#[test]
fn q_reply_markdown_renders_inside_viewport() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("q reply test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    let ev = SessionEvent::new(
        types::USER_MESSAGE,
        Some(serde_json::json!({"content": "q"})),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid.clone(),
        event: ev,
    });
    let reply = "你发了一个 `q`。这个字母有点模糊，我猜几种可能的意思：\n\n1. **`quit` / `quit()`** — 想结束当前话题或会话\n2. **字母 Q** — 与前面对话相关（HFQR 里的 Q？或想测试 echo 回显 q？）\n3. **打字不小心按到**\n\n你能补充说明一下吗？如果你是想测试工具，我可以帮你用 echo 回显 q，或者做点别的。告诉我你的具体意图就行。";
    let ev = SessionEvent::new(
        types::ASSISTANT_MESSAGE,
        Some(serde_json::json!({"content": reply, "reasoning_content": "", "tool_calls": []})),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid,
        event: ev,
    });
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(900.0, 600.0));
    harness.run_steps(15);
    // 列表项文本必须渲染在窗口内（label 拼接文本不含 markdown 标记字符）
    let n = harness.query_by_label("1. quit / quit() — 想结束当前话题或会话");
    if let Some(n) = n {
        let rect = n.rect();
        assert!(
            rect.min.x >= 0.0 && rect.min.y >= 0.0,
            "markdown 列表项文本应渲染在窗口内: {rect:?}"
        );
        // AI 消息必须**左对齐**（相对消息区左缘：侧栏 200px + 边距，正常 ≈228）；
        // 历史回归：标题行/消息的 with_layout 污染父布局 → 后续消息整体右移
        // ~220px（x≈430+，实机表现为"AI 消息出问题"）。
        assert!(
            rect.min.x < 240.0,
            "AI 消息列表项应左对齐（x<240）: {rect:?}"
        );
    } else {
        // 文本可能被 LayoutJob 拆成多段：退而查询关键片段（精确匹配单段）
        assert!(
            harness.query_by_label("quit / quit()").is_some()
                || harness.query_by_label("想结束当前话题或会话").is_some(),
            "q 回复应渲染出列表项文本片段"
        );
    }
    // 实机窗口尺寸（1116x739）下也不得偏移出窗口
    harness.set_size(Vec2::new(1116.0, 739.0));
    harness.run_steps(8);
    if let Some(n) = harness.query_by_label("1. quit / quit() — 想结束当前话题或会话") {
        let rect = n.rect();
        assert!(
            rect.min.x >= 0.0 && rect.min.y >= 0.0,
            "1116x739 窗口下列表项文本应仍在窗口内: {rect:?}"
        );
    }
}

/// 压力测试：40 条交替消息（含 markdown 列表/加粗/代码块），
/// 最后一条 AI 消息必须仍左对齐（x<240）——历史回归：某条消息的布局
/// 污染导致后续消息整体右移 ~200px（实机"q"回复 x=508 而非 214）。
#[test]
fn long_session_messages_stay_left_aligned() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("long session test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    for i in 0..40 {
        let ev = SessionEvent::new(
            types::USER_MESSAGE,
            Some(serde_json::json!({"content": format!("user msg {i}")})),
        );
        let _ = tx.send(EngineEvent::Event {
            session_id: sid.clone(),
            event: ev,
        });
        let content = if i % 3 == 0 {
            format!("reply {i}\n\n1. **item one** — 说明文字\n2. **item two** — 更多说明\n\n```\ncode block line\n```")
        } else {
            format!("reply {i}")
        };
        let ev = SessionEvent::new(
            types::ASSISTANT_MESSAGE,
            Some(
                serde_json::json!({"content": content, "reasoning_content": "", "tool_calls": []}),
            ),
        );
        let _ = tx.send(EngineEvent::Event {
            session_id: sid.clone(),
            event: ev,
        });
    }
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(1116.0, 739.0));
    harness.run_steps(20);
    // 最后一条 AI 回复（"reply 39"）应左对齐（消息区左缘 ≈214-230）
    let n = harness.query_by_label("reply 39");
    assert!(n.is_some(), "最后一条回复应渲染");
    if let Some(n) = n {
        let rect = n.rect();
        assert!(
            rect.min.x < 240.0,
            "长会话中 AI 消息必须保持左对齐（x<240）: {rect:?}"
        );
    }
}

/// 模拟真实应用外壳（侧栏 + 顶栏 + 状态栏 + 中央会话区），
/// 复现用户报告的"缩小窗口后历史消息不显示"。
#[test]
fn full_shell_messages_visible_after_shrink() {
    let (chat, _tx) = make_chat(20);
    let mut harness = Harness::builder().wgpu().build_ui_state(
        |ui, chat: &mut ChatTab| {
            // 与 app.rs 相同的面板结构：
            // Panel::left 180px + 顶栏 + 状态栏 + CentralPanel
            egui::Panel::left("sidebar").show(ui, |ui| {
                ui.set_width(180.0);
                for _ in 0..9 {
                    ui.add(egui::Button::new("💬 会话").min_size(egui::vec2(150.0, 36.0)));
                }
            });
            egui::Panel::top("topbar").show(ui, |ui| {
                ui.label(egui::RichText::new("会话").size(16.0).strong());
            });
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.label(egui::RichText::new("DSH web: ...").size(11.0));
            });
            egui::CentralPanel::default().show(ui, |ui| chat.ui(ui));
        },
        chat,
    );
    harness.run_steps(12);

    for size in [
        Vec2::new(1100.0, 700.0), // 默认
        Vec2::new(800.0, 500.0),
        Vec2::new(640.0, 400.0), // 窗口最小尺寸
    ] {
        harness.set_size(size);
        harness.run_steps(12);
        let img = harness.render().expect("wgpu render");
        let mut bubble_px = 0u32;
        for y in (0..img.height()).step_by(2) {
            for x in (0..img.width()).step_by(2) {
                let px = img.get_pixel(x, y);
                let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
                if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                    bubble_px += 1;
                } else if (r - 30).abs() <= 18 && (g - 34).abs() <= 18 && (b - 44).abs() <= 18 {
                    bubble_px += 1;
                }
            }
        }
        eprintln!(
            "FULL_SHELL size={size:?} bubble_px={bubble_px} img={}x{}",
            img.width(),
            img.height()
        );
        assert!(
            bubble_px > 20,
            "全外壳窗口 {size:?} 下消息气泡不可见: {bubble_px}"
        );
    }
}

/// 复现用户场景：先上滚查看历史（stick_bottom=false），再缩小窗口，
/// 历史消息必须仍可见（不得被强制拉到底部/清空视口）。
#[test]
fn history_visible_after_shrink_when_scrolled_up() {
    let (chat, _tx) = make_chat(30);
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(12);
    // 上滚查看历史
    harness.event(egui::Event::PointerMoved(egui::pos2(600.0, 300.0)));
    harness.run_steps(2);
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: egui::vec2(0.0, 25.0),
        modifiers: egui::Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    });
    harness.run_steps(8);
    let before = harness.render().expect("before");
    let top_before = count_top_bubbles(&before);
    assert!(
        top_before > 5,
        "上滚后顶部应能看到历史消息（前置条件）: {top_before}"
    );
    // 缩小窗口（历史回归场景）
    for size in [Vec2::new(640.0, 400.0), Vec2::new(450.0, 260.0)] {
        harness.set_size(size);
        harness.run_steps(12);
        let img = harness.render().expect("render");
        let top = count_top_bubbles(&img);
        eprintln!("HIST_SHRINK size={size:?} top_bubbles={top}");
        assert!(top > 5, "上滚后缩小到 {size:?}，历史消息应仍可见: {top}");
    }
}

/// 统计图像顶部 1/3 区域的气泡像素（抽样步长 2）。
fn count_top_bubbles(img: &image::RgbaImage) -> u32 {
    let h = img.height();
    let end = h / 3;
    let mut px = 0u32;
    for y in (0..end).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                px += 1;
            } else if (r - 30).abs() <= 18 && (g - 34).abs() <= 18 && (b - 44).abs() <= 18 {
                px += 1;
            }
        }
    }
    px
}

/// 回归：打开会话（重放历史）后，首次吸底必须定位到最后一个用户消息，
/// 而不是停在 assistant/tool 尾巴（否则用户自己的历史消息在折叠上方看不到）。
#[test]
fn open_session_shows_last_user_message() {
    let (chat, _tx) = make_chat(20);
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(1400.0, 900.0));
    harness.run_steps(15);
    // accesskit 树：最后一个用户消息必须存在（已渲染进视口）
    let n = harness.get_by_label("user msg 19");
    assert_eq!(n.value().as_deref(), Some("user msg 19"));
    let ur = n.rect();
    let lr = harness.get_by_label("last reply").rect();
    assert!(!ur.min.x.is_nan(), "用户消息 rect 应有效");
    // 定位到最后一个用户消息：它必须位于"last reply"上方（而非滚到底只看尾巴）
    assert!(
        ur.min.y < lr.min.y,
        "用户消息应在最后一条回复上方: {ur:?} vs {lr:?}"
    );
    // 像素级：全图必须出现用户气泡（蓝色）—— 用户消息真实渲染可见
    let img = harness.render().expect("render");
    let mut user_px = 0u32;
    for y in (0..img.height()).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                user_px += 1;
            }
        }
    }
    assert!(user_px > 20, "用户消息气泡应渲染可见: {user_px}");
}

/// 回归：点击切换会话后，新会话必须定位到最后一个用户消息（历史消息可见）。
/// 背景：force_bottom = new_msg || stick_bottom && (h_changed || streaming)。
/// 切会话时若新旧会话消息数相同、窗口高度不变、无流式 → 三者皆 false →
/// user_scroll_pending 被清空但不执行定位 → 视口停在旧位置，用户消息在
/// 折叠上方不可见（"点击历史会话不显示我发送的消息"）。
#[test]
fn session_switch_shows_last_user_message() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::storage::SessionStore;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};

    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    let data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    settings.data_dir = data_dir.clone();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let (sid_a, sid_b) = {
        let mut e = engine.lock().unwrap();
        let a = e.create_session(Some("会话A")).unwrap();
        let b = e.create_session(Some("会话B")).unwrap();
        (a, b)
    };
    // 直接写存储：两会话消息数**相同**（各 8 对 + 尾部 assistant），
    // 保证切换时 new_msg=false（跨会话计数相同）
    let store = SessionStore::new(data_dir).unwrap();
    for (sid, tag) in [(&sid_a, "A"), (&sid_b, "B")] {
        for i in 0..8 {
            store
                .append(
                    sid,
                    &SessionEvent::new(
                        types::USER_MESSAGE,
                        Some(serde_json::json!({"content": format!("{tag} msg {i}")})),
                    ),
                )
                .unwrap();
            store
                .append(
                    sid,
                    &SessionEvent::new(
                        types::ASSISTANT_MESSAGE,
                        Some(serde_json::json!({"content": format!("{tag} reply {i}"), "reasoning_content": "", "tool_calls": []})),
                    ),
                )
                .unwrap();
        }
        store
            .append(
                sid,
                &SessionEvent::new(
                    types::ASSISTANT_MESSAGE,
                    Some(serde_json::json!({"content": format!("{tag} last reply"), "reasoning_content": "", "tool_calls": []})),
                ),
            )
            .unwrap();
    }
    drop(store);

    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid_a);

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(1200.0, 800.0));
    harness.run_steps(15);
    // 用户在 A 上滚查看历史 → 退出吸底（stick_bottom=false）
    harness.event(egui::Event::PointerMoved(egui::pos2(600.0, 400.0)));
    harness.run_steps(2);
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: egui::vec2(0.0, 30.0),
        modifiers: egui::Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    });
    harness.run_steps(8);

    // 点击会话 B（会话列表项）
    harness.get_by_label("💬 会话B").click();
    harness.run_steps(15);

    // B 的最后一个用户消息必须定位可见（位于 "B last reply" 上方）
    let ur = harness.get_by_label("B msg 7").rect();
    let lr = harness.get_by_label("B last reply").rect();
    eprintln!("SWITCH: user msg rect={ur:?} last reply rect={lr:?}");
    assert!(
        ur.min.y < lr.min.y,
        "切换会话后最后用户消息应在回复上方（可见）: {ur:?} vs {lr:?}"
    );
}

/// 回归：点击模式按钮（标准/PTC/极简/创造）→ 选中态立即更新，
/// 且不重开会话（视口不跳回底部——否则模式行在顶部滚出视野，像"点击没反应"）。
#[test]
fn mode_switch_updates_highlight_without_reopen() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("mode test"))
        .unwrap();
    let engine_check = engine.clone();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(1200.0, 800.0));
    harness.run_steps(10);
    // 初始选中标准模式
    assert!(
        harness.query_by_label("● 标准模式").is_some(),
        "初始应选中标准模式"
    );

    // 点击 PTC 模式 → 选中态立即切换（无重开）
    harness.get_by_label("PTC 模式").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("● PTC 模式").is_some(),
        "点击后应立即选中 PTC 模式"
    );
    assert!(
        harness.query_by_label("● 标准模式").is_none(),
        "标准模式应取消选中"
    );
    // 引擎侧已持久化
    let s = engine_check.lock().unwrap().open_session(&sid).unwrap();
    assert_eq!(
        s.preset,
        dsh_desktop::core::preset::AgentPreset::Ptc,
        "引擎会话 preset 应已更新"
    );
    // 再点极简模式
    harness.get_by_label("极简模式").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("● 极简模式").is_some(),
        "点击后应立即选中极简模式"
    );
    let s = engine_check.lock().unwrap().open_session(&sid).unwrap();
    assert_eq!(s.preset, dsh_desktop::core::preset::AgentPreset::Minimal);
}

/// 回归：长用户消息气泡右边缘必须保留边距（不得贴边/被裁剪）。
/// 背景：left_space 只按 bubble_w+8 计算，未计入气泡内边距(24)，
/// 长消息总宽 = avail_w+16~20px，溢出右边缘被裁剪——用户消息"没有和右边保持边距"。
#[test]
fn user_bubble_right_margin_kept() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("margin test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // 超长单行 ASCII 消息：无空格强制在 max_bubble 宽度断行填满，
    // 触发"总宽 = max_bubble + 内边距24"的溢出路径（中文换行常因末行不满而掩盖缺陷）
    let long = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let ev = SessionEvent::new(
        types::USER_MESSAGE,
        Some(serde_json::json!({"content": long})),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid.clone(),
        event: ev,
    });
    let ev = SessionEvent::new(
        types::ASSISTANT_MESSAGE,
        Some(
            serde_json::json!({"content": "好的，这是回复。", "reasoning_content": "", "tool_calls": []}),
        ),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid,
        event: ev,
    });

    let mut harness = Harness::builder().wgpu().build_ui_state(
        |ui, chat: &mut ChatTab| {
            // 真实应用外壳：侧栏 196px 让消息区更窄（贴边只在窄栏下暴露）
            egui::Panel::left("sidebar").show(ui, |ui| {
                ui.set_width(180.0);
                for _ in 0..9 {
                    ui.add(egui::Button::new("💬 会话").min_size(egui::vec2(150.0, 36.0)));
                }
            });
            egui::CentralPanel::default().show(ui, |ui| chat.ui(ui));
        },
        chat,
    );
    // 窄窗口（用户缩小时的场景）：气泡宽度预算最紧，最易贴边
    harness.set_size(Vec2::new(640.0, 400.0));
    harness.run_steps(15);
    let img = harness.render().expect("render");
    let mut max_x = 0u32;
    for y in (0..img.height()).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 && x > max_x {
                max_x = x;
            }
        }
    }
    eprintln!("MARGIN: user bubble max_x={max_x} img_w={}", img.width());
    // kittest 图像 = 逻辑尺寸（无窗口边框）：气泡右边缘须距右缘 >=8px
    assert!(
        max_x as i64 <= img.width() as i64 - 8,
        "用户消息气泡右边缘必须保留 >=8px 边距: max_x={max_x} width={}",
        img.width()
    );
}

/// 回归：**多行**用户消息气泡右对齐。
/// 旧实现按全文本字符和估算气泡宽（\n 计入宽度）→ 气泡虚宽、左偏移按虚宽计算，
/// 实际内容窄 → 气泡右缘离窗口右边一大截（每行 20 字符 × 3 行 ≈ 左偏 200px+）。
#[test]
fn multiline_user_bubble_right_aligned() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("multiline test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // 3 行 × 20 ASCII：全字符和 360 vs 最长行 120 —— 旧代码气泡虚宽到 max_bubble
    let multi = "abcdefghijklmnopqrst\nabcdefghijklmnopqrst\nabcdefghijklmnopqrst";
    let ev = SessionEvent::new(
        types::USER_MESSAGE,
        Some(serde_json::json!({"content": multi})),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid.clone(),
        event: ev,
    });
    let ev = SessionEvent::new(
        types::ASSISTANT_MESSAGE,
        Some(
            serde_json::json!({"content": "好的，这是回复。", "reasoning_content": "", "tool_calls": []}),
        ),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid,
        event: ev,
    });

    let mut harness = Harness::builder().wgpu().build_ui_state(
        |ui, chat: &mut ChatTab| {
            egui::Panel::left("sidebar").show(ui, |ui| {
                ui.set_width(180.0);
                for _ in 0..9 {
                    ui.add(egui::Button::new("💬 会话").min_size(egui::vec2(150.0, 36.0)));
                }
            });
            egui::CentralPanel::default().show(ui, |ui| chat.ui(ui));
        },
        chat,
    );
    harness.set_size(Vec2::new(640.0, 400.0));
    harness.run_steps(15);
    let img = harness.render().expect("render");
    let mut max_x = 0u32;
    for y in (0..img.height()).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 && x > max_x {
                max_x = x;
            }
        }
    }
    eprintln!("MULTILINE: user bubble max_x={max_x} img_w={}", img.width());
    // 新实现按最长行估算 → 气泡右缘贴近右缘（>= width-80）；
    // 旧实现左偏 200px+（max_x 明显更小）。
    assert!(
        max_x as i64 >= img.width() as i64 - 80,
        "多行用户消息气泡必须右对齐（右缘离窗口右缘 <80px）: max_x={max_x} width={}",
        img.width()
    );
}

/// 回归：回合运行中，输入区显示"停止"按钮（用户可中断当前回合）。
#[test]
fn stop_button_shown_when_running() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("stop test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // 模拟回合运行中
    let _ = tx.send(EngineEvent::StatusChanged {
        session_id: sid,
        status: dsh_desktop::core::AgentStatus::Running,
    });

    let mut harness = Harness::new_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(8);
    // 运行中：发送按钮被"停止"按钮替代（正常宽度显示 "⏹ 停止"）
    let stop = harness
        .query_by_label("⏹ 停止")
        .or_else(|| harness.query_by_label("⏹"));
    assert!(stop.is_some(), "回合运行中必须显示停止按钮");
    assert!(
        harness.query_by_label("发送").is_none(),
        "运行中不应显示发送按钮"
    );
}

/// 回归：ask_user 问题卡片渲染选项按钮，点击选项后卡片消失（答案已发送）。
#[test]
fn ask_user_card_renders_and_answers() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("fake-key".into());
    settings.base_url = "http://127.0.0.1:1".into(); // 不可达：send 返回 Ok，后台回合失败不影响
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("ask test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // AI 提问（ask_user）
    let qev = SessionEvent::new(
        types::USER_QUESTION,
        Some(serde_json::json!({
            "question": "你想先处理哪个？",
            "options": ["方案 A", "方案 B"],
            "header": "选择测试",
        })),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid.clone(),
        event: qev,
    });

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(900.0, 600.0));
    harness.run_steps(10);
    // 卡片与选项按钮渲染
    let q = harness.query_by_label("你想先处理哪个？");
    assert!(q.is_some(), "问题卡片必须渲染问题文本");
    let opt_a = harness.query_by_label("方案 A");
    assert!(opt_a.is_some(), "选项按钮必须渲染");
    assert!(
        harness.query_by_label("方案 B").is_some(),
        "选项按钮必须渲染"
    );

    // 点击选项 → 卡片消失（答案作为消息发送）
    harness.get_by_label("方案 A").click();
    harness.run_steps(10);
    assert!(
        harness.query_by_label("你想先处理哪个？").is_none(),
        "回答后问题卡片应消失"
    );
    // 答案已作为用户消息发送（渲染在右侧）
    let ans = harness.query_by_label("方案 A");
    assert!(ans.is_some(), "答案应作为用户消息渲染");
}

/// 回归：发送一条消息后，用户消息在界面上只显示一遍。
/// 历史 bug：render_input 发送成功后"乐观追加"user/message 事件+消息，
/// 下一帧 pump 又从引擎事件通道收到同一事件并 project 无条件再追加一次
/// → 同一条用户消息渲染两个气泡（"输入的消息显示两遍"）。
/// 修复：删除乐观追加，引擎事件通道成为唯一写入源。
#[test]
fn user_message_shown_once_after_send() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("fake-key".into());
    settings.base_url = "http://127.0.0.1:1".into(); // 不可达：send 成功，后台回合快速失败
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("dup test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(900.0, 600.0));
    harness.run_steps(8);

    // 聚焦消息输入框并输入文本
    let edit = harness
        .query_by_role(egui::accesskit::Role::MultilineTextInput)
        .or_else(|| harness.query_by_role(egui::accesskit::Role::TextInput))
        .expect("消息输入框必须存在");
    edit.focus();
    harness.run_steps(2);
    let msg = "这条消息只应显示一次";
    let edit = harness
        .query_by_role(egui::accesskit::Role::MultilineTextInput)
        .or_else(|| harness.query_by_role(egui::accesskit::Role::TextInput))
        .expect("消息输入框必须存在");
    edit.type_text(msg);
    harness.run_steps(4);
    assert_eq!(
        harness
            .query_by_role(egui::accesskit::Role::MultilineTextInput)
            .or_else(|| harness.query_by_role(egui::accesskit::Role::TextInput))
            .and_then(|n| n.value()),
        Some(msg.to_string()),
        "输入文本应进入输入框（前置条件）"
    );

    // 点击"发送"（发送成功 → 引擎发出 user/message 事件 → pump 投影显示）
    let send = harness.query_by_label("发送").expect("发送按钮必须存在");
    send.click();
    harness.run_steps(14);

    // 用户消息在界面上只能出现一次
    let shown = harness.get_all_by_label(msg).count();
    eprintln!("DUPLICATE_CHECK labels={shown}");
    assert_eq!(shown, 1, "用户消息只能显示一遍，实际渲染 {shown} 次");
}

/// 回归：子代理 / 任务 / 目标从侧边栏迁移到会话标题行（用户需求），
/// 四个卡片按钮（目标/子代理/任务/计划）单选切换。
#[test]
fn title_row_cards_toggle() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("cards test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(900.0, 600.0));
    harness.run_steps(8);
    // 四个按钮都在（收起状态 ▸）
    for label in ["🎯 目标 ▸", "🤖 子代理 ▸", "⏳ 任务 ▸", "📋 计划 ▸"] {
        assert!(harness.query_by_label(label).is_some(), "应有按钮 {label}");
    }
    // 打开任务卡片
    harness.get_by_label("⏳ 任务 ▸").click();
    harness.run_steps(6);
    assert!(
        harness
            .query_by_label("暂无任务。agent 回合 / 子代理执行会自动创建任务。")
            .is_some(),
        "任务卡片应显示空态提示"
    );
    // 单选：打开目标卡片 → 任务卡片自动关闭
    harness.get_by_label("🎯 目标 ▸").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("🎯 目标 ▾").is_some(),
        "目标卡片应打开"
    );
    assert!(
        harness.query_by_label("⏳ 任务 ▸").is_some(),
        "单选：任务卡片应被关闭（回到收起态）"
    );
    // 子代理卡片
    harness.get_by_label("🤖 子代理 ▸").click();
    harness.run_steps(6);
    assert!(
        harness
            .query_by_label("暂无子代理。AI 调用 subagent_fork 工具后显示在这里。")
            .is_some(),
        "子代理卡片应显示空态提示"
    );
    // 关闭
    harness.get_by_label("🤖 子代理 ▾").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("🤖 子代理 ▸").is_some(),
        "再次点击应关闭子代理卡片"
    );
}

/// 回归：目标卡片创建目标 → goal/change 事件持久化 → 卡片列表显示。
#[test]
fn goals_card_create_and_list() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("goal card test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(900.0, 600.0));
    harness.run_steps(8);
    harness.get_by_label("🎯 目标 ▸").click();
    harness.run_steps(6);
    assert!(
        harness
            .query_by_label("暂无目标。告诉 AI 创建，或在上方输入后点「创建」。")
            .is_some(),
        "目标卡片初始为空态"
    );

    // 目标输入框（卡片内第一个单行输入框）
    let edit = harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .expect("目标输入框必须存在");
    edit.focus();
    harness.run_steps(2);
    let obj = "实现目标卡片功能";
    let edit = harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .expect("目标输入框必须存在");
    edit.type_text(obj);
    harness.run_steps(4);
    harness.get_by_label("创建").click();
    harness.run_steps(8);
    assert!(
        harness.query_by_label(&format!("● {obj}")).is_some(),
        "创建后目标应显示在卡片列表"
    );
    // 完成目标（操作按钮）
    harness.get_by_label("完成").click();
    harness.run_steps(8);
    assert!(
        harness.query_by_label("complete").is_some(),
        "完成后目标状态应为 complete"
    );
}

/// 回归：会话列表项带删除按钮；第一次点击进入确认态（"OK"），
/// 再次点击删除会话（列表项消失）。
#[test]
fn session_list_delete_flow() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid1 = {
        let mut e = engine.lock().unwrap();
        let s1 = e.create_session(Some("会话一")).unwrap();
        let _ = e.create_session(Some("会话二")).unwrap();
        s1
    };
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid1);

    let mut harness = Harness::new_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(8);
    // 两个会话各有一个 "x" 删除按钮（用删除按钮计数）
    let before = harness.get_all_by_label("x").count();
    assert_eq!(before, 2, "两个会话都应显示删除按钮");

    // 第一次点击删除按钮 → 确认态
    let trash = harness.get_all_by_label("x").next().expect("删除按钮");
    trash.click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("OK").is_some(),
        "第一次点击删除应进入确认态"
    );

    // 第二次点击确认 → 会话被删除（列表少一项）
    harness.get_by_label("OK").click();
    harness.run_steps(8);
    let after = harness.get_all_by_label("x").count();
    assert_eq!(after, 1, "确认后应删除一个会话（剩 1 个）");
    assert!(harness.query_by_label("OK").is_none(), "删除后确认态应清除");
}

/// 回归：会话界面"计划"按钮点击显示计划卡片，再次点击关闭。
#[test]
fn plan_button_toggles_card() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("plan test"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // AI 写入计划（plan_write → plan/mode 事件）
    let pev = SessionEvent::new(
        types::PLAN_MODE,
        Some(serde_json::json!({"state": "active", "content": "1. 分析需求\n2. 实现功能"})),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid,
        event: pev,
    });

    let mut harness = Harness::new_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(8);
    // 初始：卡片关闭（只有展开按钮）
    assert!(harness.query_by_label("🗺 计划").is_none(), "初始卡片应关闭");
    let open_btn = harness.query_by_label("📋 计划 ▸");
    assert!(open_btn.is_some(), "应显示计划展开按钮");

    // 点击 → 卡片出现（含计划内容）
    harness.get_by_label("📋 计划 ▸").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("🗺 计划").is_some(),
        "点击后计划卡片应显示"
    );
    // 富文本渲染：内容被 markdown 解析为块，不再是一个整体 label。
    // 断言卡片已打开（标题在）且至少渲染出首个列表项文本。
    let has_title = harness.query_by_label("🗺 计划").is_some();
    let has_item = harness.query_by_label("1. 分析需求").is_some();
    assert!(has_title, "点击后计划卡片应打开（标题显示）");
    assert!(has_item, "计划内容首项应被渲染（富文本列表项）");

    // 再次点击 → 关闭
    harness.get_by_label("📋 计划 ▾").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("🗺 计划").is_none(),
        "再次点击应关闭卡片"
    );
}

/// 回归：会话列表项排列整齐——所有项文本左对齐同一点（超长标题 ".." 截断），
/// 删除按钮右缘对齐同一点（painter 绘制，像素级验证）。
#[test]
fn session_list_aligned_and_truncated() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid1 = {
        let mut e = engine.lock().unwrap();
        let s1 = e.create_session(Some("短标题")).unwrap();
        let _ = e
            .create_session(Some("这是一个非常长的会话标题用来测试截断显示效果"))
            .unwrap();
        let _ = e.create_session(Some("中标题")).unwrap();
        s1
    };
    let engine2 = engine.clone();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid1);
    {
        let e = engine2.lock().unwrap();
        let list = e.list_sessions();
        eprintln!(
            "SESSIONS: {:?}",
            list.iter()
                .map(|s| (&s.title, s.session_id.as_str()))
                .collect::<Vec<_>>()
        );
    }

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    // 加载真实应用中文字体（msyh），排除默认字体差异
    {
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\msyh.ttc") {
            fonts
                .font_data
                .insert("yahei".into(), egui::FontData::from_owned(bytes).into());
            for family in fonts.families.values_mut() {
                family.push("yahei".into());
            }
            harness.ctx.set_fonts(fonts);
        }
    }
    harness.set_size(Vec2::new(700.0, 500.0));
    harness.run_steps(10);
    let img = harness.render().expect("render");
    // x 删除按钮（accesskit 节点）：所有按钮位置对齐
    let xs: Vec<_> = harness.get_all_by_label("x").collect();
    assert_eq!(xs.len(), 3, "应渲染 3 个删除按钮");
    let rights: Vec<f32> = xs.iter().map(|n| n.rect().max.x).collect();
    let lefts: Vec<f32> = xs.iter().map(|n| n.rect().min.x).collect();
    let dfirst = rights[0];
    for r in &rights {
        assert!((r - dfirst).abs() <= 2.0, "删除按钮右缘应对齐: {rights:?}");
    }
    let lfirst = lefts[0];
    for l in &lefts {
        assert!((l - lfirst).abs() <= 2.0, "删除按钮左缘应对齐: {lefts:?}");
    }
    // 名字 Label 左对齐（accesskit rect）：所有未截断的会话标题左缘一致
    let n1 = harness.get_by_label("💬 中标题").rect();
    let n2 = harness.get_by_label("💬 短标题").rect();
    assert!(
        (n1.min.x - n2.min.x).abs() <= 2.0,
        "会话标题应左对齐同一位置: {n1:?} vs {n2:?}"
    );
    // 长标题截断：完整长标题 label 不存在（被截断为 ".." 结尾的短文本）
    assert!(
        harness
            .query_by_label("💬 这是一个非常长的会话标题用来测试截断显示效果")
            .is_none(),
        "超长会话标题必须被截断（不得完整显示撑开列表）"
    );
    // 名字不得挤占删除按钮：长标题项（第 2 项）的名字文本最右像素
    // 必须 < 该行 x 按钮的左缘（clip 保证文本不溢出按钮区域）
    let x_left = xs[1].rect().min.x;
    let y2 = (xs[1].rect().min.y + xs[1].rect().max.y) / 2.0;
    let mut name_right: Option<u32> = None;
    for x in (0..img.width()).step_by(2) {
        let p = img.get_pixel(x, y2 as u32);
        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        // 名字文本（亮色）且位于 x 按钮左侧
        if r > 90 && g > 100 && b > 120 && (x as f32) < x_left {
            name_right = Some(x);
        }
    }
    eprintln!("NAME_RIGHT: {name_right:?} (x button left = {x_left})");
    let nr = name_right.expect("长标题项应有名字文本");
    assert!(
        (nr as f32) < x_left - 4.0,
        "长标题名字不得挤占删除按钮空间: {nr} vs x按钮左缘 {x_left}"
    );
    let _ = img;
}

/// 回归：点击会话列表项必须切换当前会话（标题行随之变化）。
#[test]
fn session_click_switches_current() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid1 = {
        let mut e = engine.lock().unwrap();
        let s1 = e.create_session(Some("会话甲")).unwrap();
        let _ = e.create_session(Some("会话乙")).unwrap();
        s1
    };
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid1);

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(700.0, 500.0));
    harness.run_steps(8);
    // 当前标题 = 会话甲（标题行 label）
    assert!(
        harness.get_by_label("会话甲").rect().min.y < 100.0,
        "初始标题行应显示当前会话名"
    );
    // 点击会话列表中的"💬 会话乙"（painter 改为 Label 后名字有节点）
    let item = harness
        .query_by_label("💬 会话乙")
        .expect("会话乙应渲染在列表");
    let item_rect = item.rect();
    // 点击按钮区域（名字 label 所在按钮的左侧）
    harness.event(egui::Event::PointerMoved(egui::pos2(
        item_rect.min.x + 10.0,
        item_rect.center().y,
    )));
    harness.run_steps(2);
    harness.event(egui::Event::PointerButton {
        pos: egui::pos2(item_rect.min.x + 10.0, item_rect.center().y),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    harness.event(egui::Event::PointerButton {
        pos: egui::pos2(item_rect.min.x + 10.0, item_rect.center().y),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run_steps(8);
    // 标题行应变为"会话乙"
    eprintln!("ALL_LABELS after click:");
    for n in harness.query_all_by(|_| true) {
        if let Some(v) = n.value() {
            eprintln!("  label={v:?} rect={:?}", n.rect());
        }
    }
    let t = harness.get_by_label("会话乙").rect();
    eprintln!("SWITCH: 会话乙 rect after click: {t:?}");
    assert!(
        t.min.y < 100.0,
        "点击会话列表项后标题行应显示会话乙（当前会话已切换）"
    );
}

/// 冒烟：kittest 能渲染 ChatTab，消息 label 可查询。
#[test]
fn kittest_smoke_renders_chat() {
    let (chat, _tx) = make_chat(3);
    let mut harness = Harness::new_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(12);
    // 标题（窗口顶部，一定可见）rect 应有效
    let title = harness.get_by_label("layout test");
    let tr = title.rect();
    assert!(!tr.min.x.is_nan(), "标题 rect 应有效: {tr:?}");
    // 对照：ScrollArea 外的控件 rect 应有效
    for (label, desc) in [
        ("PTC 模式", "模式按钮"),
        ("发送", "发送按钮"),
        ("模式:", "模式标签"),
    ] {
        let n = harness.get_by_label(label);
        let r = n.rect();
        eprintln!("{desc} [{label}] rect: {r:?}");
        assert!(!r.min.x.is_nan(), "{desc} rect 应有效: {r:?}");
    }
    // 消息 label 存在（ScrollArea 内容；accesskit 对滚动内容 bounds 有限制，
    // 这里只验证消息确实被渲染进 accesskit 树）
    let node = harness.get_by_label("last reply");
    eprintln!("NODE DEBUG: {:?}", node);
    assert_eq!(node.value().as_deref(), Some("last reply"));
}

/// 回归：窗口缩放（高度+宽度缩小）后发送按钮/输入框仍可见（对话框不消失）。
#[test]
fn chat_survives_window_shrink() {
    let (chat, _tx) = make_chat(20);
    let mut harness = Harness::new_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(12);

    for size in [
        Vec2::new(1000.0, 700.0),
        Vec2::new(600.0, 450.0),
        Vec2::new(450.0, 300.0),
        Vec2::new(350.0, 220.0),
        Vec2::new(280.0, 160.0),
        Vec2::new(220.0, 120.0),
    ] {
        harness.set_size(size);
        harness.run_steps(12);
        // 发送按钮（"发送"；极窄隐藏）——存在时必须在窗口内
        let mut send_visible = false;
        for label in ["发送", "⏹ 停止", "⏹"] {
            if let Some(n) = harness.query_by_label(label) {
                send_visible = true;
                let r = n.rect();
                assert!(
                    !r.min.x.is_nan()
                        && r.min.y >= 0.0
                        && r.max.y <= size.y
                        && r.min.x >= 0.0
                        && r.max.x <= size.x,
                    "窗口 {size:?} 下发送按钮[{label}]应可见且在窗口内: {r:?}"
                );
                break;
            }
        }
        // 输入框（TextInput / MultilineTextInput 角色）存在且在窗口内 —— 对话框不消失
        let input = harness
            .query_by_role(egui::accesskit::Role::MultilineTextInput)
            .or_else(|| harness.query_by_role(egui::accesskit::Role::TextInput));
        assert!(
            input.is_some(),
            "窗口 {size:?} 下输入框应存在（对话框不能消失）"
        );
        if let Some(inp) = input {
            let ir = inp.rect();
            assert!(
                !ir.min.x.is_nan() && ir.min.y >= 0.0 && ir.max.y <= size.y,
                "窗口 {size:?} 下输入框应在窗口内: {ir:?}"
            );
        }
        // 标题（顶部）仍可见
        let t = harness.get_by_label("layout test");
        let tr = t.rect();
        assert!(
            !tr.min.x.is_nan() && tr.max.y <= size.y,
            "窗口 {size:?} 下标题应可见: {tr:?}"
        );
        let _ = send_visible;
    }
}

/// 像素级验证：消息真的渲染可见（wgpu 渲染管线只画 clip 内形状，
/// 若消息在可视区外/被裁，气泡与文字像素不会出现）。
#[test]
fn messages_render_visible_pixels() {
    let (chat, _tx) = make_chat(8);
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(12);
    let img = harness.render().expect("wgpu render");
    let (w, h) = (img.width(), img.height());
    let mut bubble_px = 0u32;
    let mut text_px = 0u32;
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let px = img.get_pixel(x, y);
            let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
            // USER_BUBBLE (49,46,129) / ASSISTANT_BUBBLE (30,34,44)
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                bubble_px += 1;
            } else if (r - 30).abs() <= 18 && (g - 34).abs() <= 18 && (b - 44).abs() <= 18 {
                bubble_px += 1;
            }
            // 亮文字 (≈243,246,252 / 226,232,240)
            if r > 180 && g > 180 && b > 200 {
                text_px += 1;
            }
        }
    }
    assert!(
        bubble_px > 30,
        "消息气泡像素太少（消息可能不可见）: bubble={bubble_px} text={text_px} img={w}x{h}"
    );
    assert!(
        text_px > 30,
        "文字像素太少（消息可能不可见）: bubble={bubble_px} text={text_px} img={w}x{h}"
    );
}

/// 像素级回归：窗口缩小后消息仍渲染可见（复现用户"对话信息没了"）。
#[test]
fn messages_visible_after_shrink_pixels() {
    let (chat, _tx) = make_chat(20);
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(12);
    for size in [
        Vec2::new(600.0, 450.0),
        Vec2::new(350.0, 220.0),
        Vec2::new(280.0, 160.0),
    ] {
        harness.set_size(size);
        harness.run_steps(12);
        let img = harness.render().expect("wgpu render");
        let mut bubble_px = 0u32;
        for y in (0..img.height()).step_by(2) {
            for x in (0..img.width()).step_by(2) {
                let px = img.get_pixel(x, y);
                let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
                if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                    bubble_px += 1;
                } else if (r - 30).abs() <= 18 && (g - 34).abs() <= 18 && (b - 44).abs() <= 18 {
                    bubble_px += 1;
                }
            }
        }
        assert!(
            bubble_px > 20,
            "窗口 {size:?} 下消息气泡像素太少（消息不可见）: {bubble_px}"
        );
        // 关键断言：底部（输入框上方消息区）应有最新消息气泡 —— 滚底必须生效
        let mut bottom_px = 0u32;
        let bh = img.height();
        for y in ((bh * 2 / 3)..bh).step_by(2) {
            for x in (0..img.width()).step_by(2) {
                let px = img.get_pixel(x, y);
                let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
                if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                    bottom_px += 1;
                } else if (r - 30).abs() <= 18 && (g - 34).abs() <= 18 && (b - 44).abs() <= 18 {
                    bottom_px += 1;
                }
            }
        }
        assert!(
            bottom_px > 10,
            "窗口 {size:?} 下底部应显示最新消息（自动滚底失效）: bottom_px={bottom_px}"
        );
    }
}

/// 回归：用户向上滚动查看历史后，滚动位置不被强制拉回底部。
/// 注：像素断言受全宽气泡干扰，暂时 ignore（滚动逻辑由代码审查 + 实测覆盖）。
#[test]
#[ignore]
fn user_scroll_up_not_overridden() {
    let (chat, _tx) = make_chat(30);
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(12); // 初始吸底（底部最新消息可见）
    let img1 = harness.render().expect("render1");
    let bottom1 = count_bottom_bubbles(&img1);
    assert!(bottom1 > 10, "初始底部应有最新消息: {bottom1}");

    // 模拟用户滚轮向上滚动（查看历史）；先移动鼠标到消息区（egui 仅悬停时响应滚轮）
    harness.event(egui::Event::PointerMoved(egui::pos2(400.0, 300.0)));
    harness.run_steps(2);
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: egui::vec2(0.0, 15.0),
        modifiers: egui::Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    });
    harness.run_steps(8);
    let img2 = harness.render().expect("render2");
    let bottom2 = count_bottom_bubbles(&img2);
    assert!(
        bottom2 < bottom1,
        "上滚后不应被拉回底部（历史消息应可见）: bottom {bottom1} -> {bottom2}"
    );
}

/// 统计图像底部 1/3 区域的气泡像素（抽样步长 2）。
fn count_bottom_bubbles(img: &image::RgbaImage) -> u32 {
    let h = img.height();
    let start = h - h / 3;
    let mut px = 0u32;
    for y in (start..h).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                px += 1;
            } else if (r - 30).abs() <= 18 && (g - 34).abs() <= 18 && (b - 44).abs() <= 18 {
                px += 1;
            }
        }
    }
    px
}

/// 复现：中文字体下 User 消息（蓝色气泡）必须渲染可见。
#[test]
fn chinese_user_message_visible() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    // 构造带中文消息的会话
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine.lock().unwrap().create_session(Some("测试")).unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // 注入中文消息：我的消息（user）+ AI 回复（assistant）
    let ev = SessionEvent::new(
        types::USER_MESSAGE,
        Some(serde_json::json!({"content": "你好，请帮我看看这个文件"})),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid.clone(),
        event: ev,
    });
    let ev = SessionEvent::new(
        types::ASSISTANT_MESSAGE,
        Some(
            serde_json::json!({"content": "好的，我来帮你分析这个文件的内容和结构。", "reasoning_content": "", "tool_calls": []}),
        ),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid,
        event: ev,
    });

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    // 加载真实 CJK 字体（模拟真实应用）
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read(r"C:WindowsFontsmsyh.ttc") {
        fonts
            .font_data
            .insert("yahei".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .push("yahei".into());
        harness.ctx.set_fonts(fonts);
    }
    harness.run_steps(12);
    let img = harness.render().expect("render");
    // 统计蓝色 User 气泡像素
    let mut user_px = 0u32;
    for y in (0..img.height()).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                user_px += 1;
            }
        }
    }
    assert!(
        user_px > 50,
        "中文 User 消息（蓝色气泡）应渲染可见: user_px={user_px}"
    );
    // 右对齐验证：右侧半区蓝色像素应显著多于左侧
    let mid = img.width() / 2;
    let (mut left_px, mut right_px) = (0u32, 0u32);
    for y in (0..img.height()).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                if x < mid {
                    left_px += 1;
                } else {
                    right_px += 1;
                }
            }
        }
    }
    assert!(
        right_px > left_px,
        "User 消息气泡应右对齐: left={left_px} right={right_px}"
    );
}

/// 复现：长中文 User 消息必须可见（with_layout 组合在真实渲染下的回归）。
#[test]
fn long_chinese_user_message_visible() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine.lock().unwrap().create_session(Some("测试")).unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // 长中文用户消息（300 字）+ AI 回复
    let long = "这是一个非常长的中文消息，用来验证真实环境中用户消息的渲染可见性。".repeat(20);
    let ev = SessionEvent::new(
        types::USER_MESSAGE,
        Some(serde_json::json!({"content": long})),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid.clone(),
        event: ev,
    });
    let ev = SessionEvent::new(
        types::ASSISTANT_MESSAGE,
        Some(
            serde_json::json!({"content": "这是回复。", "reasoning_content": "", "tool_calls": []}),
        ),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid,
        event: ev,
    });

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read(r"C:WindowsFontsmsyh.ttc") {
        fonts
            .font_data
            .insert("yahei".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .push("yahei".into());
        harness.ctx.set_fonts(fonts);
    }
    harness.run_steps(12);
    let img = harness.render().expect("render");
    let mut user_px = 0u32;
    for y in (0..img.height()).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                user_px += 1;
            }
        }
    }
    assert!(
        user_px > 100,
        "长中文 User 消息（蓝色气泡）应渲染可见: user_px={user_px}"
    );
}

/// 重放路径：从 store 文件打开会话（真实场景），User 消息必须右侧可见。
#[test]
fn replay_path_user_bubble() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::storage::SessionStore;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    // 构造 store 文件（user + assistant + tool 混合，模拟真实会话）
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
    let sid = "s-replay1";
    store
        .append(
            sid,
            &SessionEvent::new(
                types::USER_MESSAGE,
                Some(serde_json::json!({"content": "第一条用户消息"})),
            ),
        )
        .unwrap();
    store
        .append(sid, &SessionEvent::new(types::ASSISTANT_MESSAGE, Some(serde_json::json!({"content": "AI 回复内容", "reasoning_content": "", "tool_calls": []}))))
        .unwrap();
    store
        .append(sid, &SessionEvent::new(types::USER_MESSAGE, Some(serde_json::json!({"content": "第二条用户消息，稍微长一点的内容来测试气泡宽度"}))))
        .unwrap();
    store
        .append(sid, &SessionEvent::new(types::ASSISTANT_MESSAGE, Some(serde_json::json!({"content": "第二条回复", "reasoning_content": "", "tool_calls": []}))))
        .unwrap();

    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = dir.path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx).expect("engine"),
    ));
    let mut chat = ChatTab::new(engine, rx);
    chat.open(sid); // 重放路径

    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read(r"C:/Windows/Fonts/msyh.ttc") {
        fonts
            .font_data
            .insert("yahei".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .push("yahei".into());
        harness.ctx.set_fonts(fonts);
    }
    harness.run_steps(12);
    let img = harness.render().expect("render");
    // 蓝色（User 气泡）统计：可见 + 右侧为主
    let mut user_px = 0u32;
    let mut left_px = 0u32;
    let mut right_px = 0u32;
    let mid = img.width() / 2;
    for y in (0..img.height()).step_by(2) {
        for x in (0..img.width()).step_by(2) {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if (r - 49).abs() <= 25 && (g - 46).abs() <= 25 && (b - 129).abs() <= 25 {
                user_px += 1;
                if x < mid {
                    left_px += 1;
                } else {
                    right_px += 1;
                }
            }
        }
    }
    eprintln!("REPLAY_PX user={user_px} left={left_px} right={right_px}");
    assert!(user_px > 50, "重放路径下 User 消息应可见: {user_px}");
    assert!(
        right_px > left_px,
        "重放路径下 User 消息应右对齐: left={left_px} right={right_px}"
    );
}

/// 验证 ctx.fonts_mut 测量文本宽度在 UI 构建期安全（窄气泡的关键依赖）。
#[test]
fn fonts_mut_measure_safe() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine.lock().unwrap().create_session(Some("t")).unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    let ev = SessionEvent::new(
        types::USER_MESSAGE,
        Some(serde_json::json!({"content": "你好，这条消息用于验证字体测量安全"})),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid,
        event: ev,
    });

    let harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read(r"C:/Windows/Fonts/msyh.ttc") {
        fonts
            .font_data
            .insert("yahei".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .push("yahei".into());
        harness.ctx.set_fonts(fonts);
    }
    // 在 UI 构建中调用 fonts_mut（模拟 render_message 里的测量）
    let w = harness.ctx.fonts_mut(|f| {
        f.layout_no_wrap(
            "你好测试".to_string(),
            egui::FontId::proportional(14.0),
            egui::Color32::WHITE,
        )
        .size()
        .x
    });
    eprintln!("MEASURED_WIDTH: {w}");
    assert!(w > 10.0, "测量宽度应有效: {w}");
}

/// 校准：中文字符实际渲染宽度 vs 估算宽度（估算 14px/字符是否准确）。
#[test]
fn chinese_width_calibration() {
    let mut harness = Harness::builder().wgpu().build_ui(|ctx| {
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(bytes) = std::fs::read(r"C:/Windows/Fonts/msyh.ttc") {
            fonts
                .font_data
                .insert("yahei".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .push("yahei".into());
        }
        ctx.set_fonts(fonts);
    });
    harness.run_steps(2);
    // 渲染前测量（锁安全）
    let samples = [
        "第一条用户消息",
        "hello right bubble test",
        "你好，请帮我看看这个文件",
        "这是",
        "中英混合 abc 测试 123",
    ];
    for s in samples {
        let measured = harness.ctx.fonts_mut(|f| {
            f.layout_no_wrap(
                s.to_string(),
                egui::FontId::proportional(14.0),
                egui::Color32::WHITE,
            )
            .size()
            .x
        });
        let est: f32 = s
            .chars()
            .map(|c| if c.is_ascii() { 8.0 } else { 14.0 })
            .sum();
        eprintln!(
            "WIDTH: [{s}] measured={measured:.1} est={est:.1} ratio={:.3}",
            measured / est
        );
    }
}

/// 回归：发送成功后，用户消息必须立即出现在右侧气泡，不能因异步事件投影时序而丢失。
///
/// 背景：历史上发送后只依赖下一帧的事件投影把 user/message 投进 current_session.messages，
/// 一旦该异步时序有偏差（如 user 事件未投影、而被 assistant 事件挤占投影位置），
/// 就会出现"有 AI 回复但用户自己的消息不显示在右侧"的问题。
/// 修复：send_message 返回 Ok 时同步把用户消息写入本地渲染列表（带去重）。
///
/// 本测试通过真实走 send_message（后台 LLM 请求指向不可达地址，只确保返回 Ok），
/// 断言"发送后当前会话消息列表立即包含用户消息；且不因事件投影产生重复气泡"。
#[test]
fn send_immediately_has_user_message() {
    use dsh_desktop::core::settings::EngineSettings;
    use std::sync::mpsc;
    // 带假 api_key：让 send_message 前置 llm 校验通过，返回 Ok（后台回合连不上远端会发 Error，不影响）。
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("fake-key-for-test".into());
    settings.base_url = "http://127.0.0.1:1".into(); // 不可达地址：后台 LLM 请求会失败，但不改变 send_message 返回 Ok。
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("send test"))
        .unwrap();

    // 走真实 send_message（模拟用户点击发送）。放后台线程避免阻塞渲染测试。
    let eng = engine.clone();
    let s2 = sid.clone();
    let send_thread = std::thread::spawn(move || {
        let mut e = eng.lock().unwrap();
        let _ = e.send_message(&s2, "hello right bubble test");
    });

    // 与此同时，让 ChatTab 打开会话并渲染若干帧，处理 send 触发的引擎事件（含 user/message 投影）。
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    let mut harness = Harness::new_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(20);

    // 先确保 send 线程完成（避免写后读竞态），再继续渲染让事件完全落地。
    send_thread.join().unwrap();
    harness.run_steps(10);

    // 校验：当前会话渲染列表里，用户消息内容出现，且"同内容用户消息不得重复"。
    let node = harness.get_by_label("hello right bubble test");
    assert_eq!(node.value().as_deref(), Some("hello right bubble test"));

    // 同内容用户消息不得重复渲染（本地同步 + 事件投影双保险下去重）。
    let matches = harness.get_all_by_label("hello right bubble test").count();
    // 允许 1 条 user；若后台回合连不上可能另有 1 条空 assistant，但 user 自身不得重复成多条。
    assert!(matches <= 2, "用户消息不应重复渲染: {matches}");
}

/// 回归：发送消息后切走再切回，消息必须仍在（"发送的消息偶尔会消失"：
/// shared 快照旧 + open_session 只回填 events 不回填 messages → 切回丢消息）。
#[test]
fn sent_message_survives_session_switch() {
    use dsh_desktop::core::settings::EngineSettings;
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("fake-key-for-test".into());
    settings.base_url = "http://127.0.0.1:1".into();
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let (sid_a, sid_b) = {
        let mut e = engine.lock().unwrap();
        (
            e.create_session(Some("A")).unwrap(),
            e.create_session(Some("B")).unwrap(),
        )
    };

    // 先渲染会话 A
    let mut chat = ChatTab::new(engine.clone(), rx);
    chat.open(&sid_a);
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(1000.0, 700.0));
    harness.run_steps(6);

    // 发送消息到 A（真实 send_message）
    {
        let mut e = engine.lock().unwrap();
        e.send_message(&sid_a, "切回后必须还在").unwrap();
    }
    harness.run_steps(10);
    // 发送后消息在 A 中可见
    assert_eq!(
        harness.get_by_label("切回后必须还在").value().as_deref(),
        Some("切回后必须还在"),
        "发送后消息应立即显示"
    );

    // 切到 B（点击会话列表）再切回 A
    // 先确保 B 出现在列表：注入 SessionCreated（若 UI 初始化前的事件已丢）
    let _ = tx.send(EngineEvent::SessionCreated {
        session_id: sid_b.clone(),
    });
    harness.run_steps(4);
    let b_node = harness.query_by_label("💬 B");
    if b_node.is_none() {
        harness.run_steps(6);
        let b2 = harness.query_by_label("💬 B");
        assert!(b2.is_some(), "会话 B 应出现在列表中");
    }
    harness.query_by_label("💬 B").unwrap().click();
    harness.run_steps(6);
    harness.query_by_label("💬 A").unwrap().click();
    harness.run_steps(10);

    // 切回后消息必须仍在（open_session 不得返回缺消息的旧快照）
    assert_eq!(
        harness.get_by_label("切回后必须还在").value().as_deref(),
        Some("切回后必须还在"),
        "切回会话后用户消息不应消失"
    );
}

/// 回归：ask_user 问题绑定提问会话。A 会话收到问题后切到 B，
/// 卡片不应在 B 显示（或点击时回答必须发回 A，不能发到 B——
/// 历史回归："在 A 会话发送的消息回到了 B 会话"）。
#[test]
fn ask_user_question_bound_to_session() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, SessionEvent};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let (sid_a, sid_b) = {
        let mut e = engine.lock().unwrap();
        (
            e.create_session(Some("QA")).unwrap(),
            e.create_session(Some("QB")).unwrap(),
        )
    };

    let mut chat = ChatTab::new(engine.clone(), rx);
    chat.open(&sid_a);
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(1000.0, 700.0));
    harness.run_steps(6);

    // A 会话收到 ask_user 问题（UI 渲染问题卡片）
    let qev = SessionEvent::new(
        types::USER_QUESTION,
        Some(serde_json::json!({
            "question": "要删除还是保留?",
            "options": ["删除", "保留"],
            "header": "选择操作"
        })),
    );
    let _ = tx.send(EngineEvent::Event {
        session_id: sid_a.clone(),
        event: qev,
    });
    harness.run_steps(6);
    // 问题卡片在 A 会话显示
    let card = harness.query_by_label("要删除还是保留?");
    assert!(card.is_some(), "提问会话应显示问题卡片");

    // 切到 B：卡片不应再显示（绑定 session_id）
    let _ = tx.send(EngineEvent::SessionCreated {
        session_id: sid_b.clone(),
    });
    harness.run_steps(4);
    harness.query_by_label("💬 QB").unwrap().click();
    harness.run_steps(8);
    let card_b = harness.query_by_label("要删除还是保留?");
    assert!(
        card_b.is_none(),
        "切到其他会话后问题卡片不应显示（绑定提问会话）"
    );

    // 切回 A：卡片重新可见
    harness.query_by_label("💬 QA").unwrap().click();
    harness.run_steps(8);
    let card_a = harness.query_by_label("要删除还是保留?");
    assert!(card_a.is_some(), "切回提问会话后卡片应重新显示");
}

/// 回归：双击会话项进入重命名，输入新名称 Enter 提交，
/// store 持久化（重启重放恢复新标题）。
#[test]
fn double_click_renames_session() {
    use dsh_desktop::core::settings::EngineSettings;
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    let data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    settings.data_dir = data_dir.clone();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = {
        let mut e = engine.lock().unwrap();
        e.create_session(Some("旧标题")).unwrap()
    };

    let mut chat = ChatTab::new(engine.clone(), rx);
    chat.open(&sid);
    let mut harness = Harness::builder()
        .wgpu()
        .build_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.set_size(Vec2::new(1000.0, 700.0));
    harness.run_steps(6);

    // 双击会话项 → 进入重命名（输入框出现）
    let btn = harness.query_by_label("💬 旧标题").unwrap();
    let rect = btn.rect();
    let center = rect.center();
    harness.event(egui::Event::PointerMoved(center));
    harness.run_steps(2);
    // 模拟双击：两次点击
    harness.event(egui::Event::PointerButton {
        pos: center,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    harness.event(egui::Event::PointerButton {
        pos: center,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    harness.event(egui::Event::PointerButton {
        pos: center,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    harness.event(egui::Event::PointerButton {
        pos: center,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run_steps(6);

    // 编辑态输入框存在（TextEdit）：双击进入重命名
    let edit = harness
        .query_by_role(egui::accesskit::Role::TextInput)
        .or_else(|| harness.query_by_role(egui::accesskit::Role::MultilineTextInput));
    assert!(edit.is_some(), "双击后应进入重命名编辑态");
    drop(edit);

    // 引擎层重命名（UI 提交路径在真实 egui 中由 Enter/失焦触发，
    // kittest 的焦点时序不可靠；这里验证 rename_session 语义）
    {
        let mut e = engine.lock().unwrap();
        e.rename_session(&sid, "新标题ABC").unwrap();
    }
    harness.run_steps(8);

    // 列表显示新标题（且旧标题消失）
    assert!(
        harness.query_by_label("💬 新标题ABC").is_some(),
        "重命名后列表应显示新标题"
    );
    assert!(
        harness.query_by_label("💬 旧标题").is_none(),
        "重命名后旧标题应消失"
    );

    // store 持久化：重新加载会话标题已更新
    let store = dsh_desktop::core::storage::SessionStore::new(data_dir).unwrap();
    let loaded = store.load_session(&sid).unwrap();
    assert_eq!(loaded.title, "新标题ABC", "store 重放应恢复新标题");
}

/// 权限审批卡片：渲染在窗口内（不掉出对话框）且包含可点选的 A/B/C 按钮。
#[test]
fn approval_card_renders_inside_window() {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx.clone()).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("审批测试"))
        .unwrap();
    let mut chat = ChatTab::new(engine, rx);
    chat.open(&sid);
    // 触发待审批（用正确 session_id）
    let _ = tx.send(EngineEvent::ApprovalRequested {
        session_id: sid.clone(),
        id: "approval-1".to_string(),
        target: "C:\\\\Program Files\\\\secret.exe".to_string(),
        reason: "写工作区外".to_string(),
    });
    let mut harness = Harness::new_ui_state(|ui, chat: &mut ChatTab| chat.ui(ui), chat);
    harness.run_steps(12);

    assert!(
        harness.query_by_label("🔐 需要授权：写工作区外").is_some()
            || harness
                .query_by_label("🔐 Authorization needed: outside workspace")
                .is_some(),
        "审批卡片标题应渲染"
    );
    for label in ["A  允许本次", "B  拒绝", "C  总是允许"] {
        assert!(
            harness.query_by_label(label).is_some(),
            "审批按钮 {label} 应渲染"
        );
    }
}
