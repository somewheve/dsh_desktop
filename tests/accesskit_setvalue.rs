//! AccessKit SetValue → 草稿 注入的端到端回归：
//! egui 0.36 TextEdit 不处理 SetValue，应用层补的消费逻辑必须真的生效
//! （外部辅助技术/自动化经 UIA ValuePattern 设值直达输入框）。
//! 发送按钮用 click_accesskit（AXPress）触发——顺带回归 egui 的
//! AccessKit Click 路径。
use dsh_desktop::core::settings::EngineSettings;
use dsh_desktop::core::{DshEngine, EngineEvent};
use dsh_desktop::ui::chat_tab::ChatTab;
use egui::Vec2;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

#[test]
fn accesskit_setvalue_reaches_draft_and_sends() {
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx).expect("engine"),
    ));
    let sid = engine
        .lock()
        .unwrap()
        .create_session(Some("a11y setvalue"))
        .unwrap();
    let mut chat = ChatTab::new(engine.clone(), rx);
    chat.open(&sid);

    let mut harness = Harness::new_ui_state(
        |ui, chat: &mut ChatTab| {
            egui::Panel::left("test_session_list")
                .exact_size(172.0)
                .show(ui, |ui| {
                    let _ = chat.ui_session_list(ui, None);
                });
            chat.ui(ui);
        },
        chat,
    );
    harness.set_size(Vec2::new(900.0, 600.0));
    harness.run_steps(6);
    // 未设值直接 AXPress 发送：草稿为空 → 不产生消息
    harness.get_by_label("发送").click_accesskit();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("来自外部自动化").is_none(),
        "空草稿发送不应产生消息"
    );

    // 注入 AccessKit SetValue（目标 = 聊天输入框的 accesskit 节点）
    let node = egui::Id::new("chat_input_edit").accesskit_id();
    harness.event(egui::Event::AccessKitActionRequest(accesskit::ActionRequest {
        action: accesskit::Action::SetValue,
        target_tree: accesskit::TreeId::ROOT,
        target_node: node,
        data: Some(accesskit::ActionData::Value("来自外部自动化".into())),
    }));
    harness.run_steps(3);

    // SetValue 直达草稿 → AXPress 发送 → 用户消息落库显示
    harness.get_by_label("发送").click_accesskit();
    harness.run_steps(10);
    assert!(
        harness.query_by_label("来自外部自动化").is_some(),
        "SetValue 应直达草稿，发送后消息显示在会话中"
    );
}
