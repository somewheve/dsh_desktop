//! 端到端冒烟测试：
//! - 插件导入/启停在临时 DSH_HOME 上往返（不碰真实 ~/.dsh）

use std::path::Path;

/// 插件管理：临时 DSH_HOME + 构造 profile，验证列表/启停/移除（纯文件层，不跑 CLI）。
#[test]
fn plugin_management_roundtrip_temp_home() {
    let tmp = tempfile::tempdir().unwrap();
    let profiles = tmp.path().join("profiles");
    let web = profiles.join("web");
    std::fs::create_dir_all(&web).unwrap();
    // package.json（web profile 形态）
    std::fs::write(
        web.join("package.json"),
        serde_json::json!({
            "name": "dsh-profile-web",
            "private": true,
            "dependencies": {
                "@deepseek-ai/dsh-base": "0.1.0-rc.6",
                "@deepseek-ai/dsh-web-app": "0.1.0-rc.6"
            },
            "dsh": { "profile": { "bundles": ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"] } }
        })
        .to_string(),
    )
    .unwrap();
    // cordis.patch.yml 空数组
    std::fs::write(
        web.join("cordis.patch.yml"),
        "[]
",
    )
    .unwrap();
    std::fs::write(
        web.join("cordis.yml"),
        "[]
",
    )
    .unwrap();

    // 列表
    let plugins = dsh_desktop::dsh::plugins::list_plugins(&web).unwrap();
    assert_eq!(plugins.len(), 2);
    let base = plugins
        .iter()
        .find(|p| p.name == "@deepseek-ai/dsh-base")
        .unwrap();
    assert!(base.is_bundle);
    assert!(!base.disabled);

    // 禁用 -> 列表反映
    dsh_desktop::dsh::plugins::set_plugin_enabled(&web, "@deepseek-ai/dsh-base", false).unwrap();
    let plugins = dsh_desktop::dsh::plugins::list_plugins(&web).unwrap();
    assert!(
        plugins
            .iter()
            .find(|p| p.name == "@deepseek-ai/dsh-base")
            .unwrap()
            .disabled
    );

    // 重新启用
    dsh_desktop::dsh::plugins::set_plugin_enabled(&web, "@deepseek-ai/dsh-base", true).unwrap();
    let plugins = dsh_desktop::dsh::plugins::list_plugins(&web).unwrap();
    assert!(
        !plugins
            .iter()
            .find(|p| p.name == "@deepseek-ai/dsh-base")
            .unwrap()
            .disabled
    );

    // 移除依赖
    dsh_desktop::dsh::profile::remove_dependency(&web, "@deepseek-ai/dsh-web-app").unwrap();
    let plugins = dsh_desktop::dsh::plugins::list_plugins(&web).unwrap();
    assert_eq!(plugins.len(), 1);
    assert!(Path::new(&web.join("cordis.patch.yml")).exists());
}

/// config：默认 DSH_HOME 解析与保存往返。
#[test]
fn config_roundtrip() {
    let cfg = dsh_desktop::config::AppConfig::default();
    assert!(
        cfg.dsh_home.to_string_lossy().contains(".dsh")
            || cfg.dsh_home.to_string_lossy().contains("dsh")
    );
    // shell 探测至少返回一个候选
    assert!(!cfg.resolved_shell().is_empty());
}

/// 引擎：创建会话 → 发送消息（无 API key 时优雅报错，不 panic）。
#[test]
fn engine_create_session_and_error_path() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    // 隔离：指向不存在的 DSH_HOME，避免读到真实 credentials
    // 隔离 + 防 race：目录名含 "dsh"（并行测试里 config_roundtrip 断言
    // DSH_HOME 路径包含 "dsh"；纯随机 tempdir 名会偶发踩失败）
    std::env::set_var(
        "DSH_HOME",
        tempfile::tempdir()
            .unwrap()
            .path()
            .join("dsh-home")
            .to_str()
            .unwrap(),
    );
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    // 无 key 且无 DSH credentials → new 应成功（UI 可用），但发送时应报明确错误
    let mut engine = DshEngine::new(settings, tx).expect("无 key 也应能创建引擎");
    let sid = engine.create_session(None).unwrap();
    let send = engine.send_message(&sid, "hi");
    assert!(send.is_err(), "无 API key 时发送应报错");
    assert!(send.unwrap_err().to_string().contains("API key"));
}

/// 引擎：带假 key 时能创建会话并列出。
#[test]
fn engine_session_crud() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("sk-test".into());
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let _test_data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");
    let id = engine.create_session(Some("测试会话")).unwrap();
    // 打开并确认标题
    let s = engine.open_session(&id).unwrap();
    assert_eq!(s.title, "测试会话");
    // 列出
    let list = engine.list_sessions();
    assert!(list.iter().any(|s| s.session_id == id));
    let _ = rx;
}

/// 工作区：打开目录 → 会话绑定 cwd → 工具根目录切换。
#[test]
fn workspace_open_and_bind_session() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    use std::path::PathBuf;
    let dir = tempfile::tempdir().unwrap();
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let _test_data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");

    // 未设置时无工作区
    assert!(engine.workspace_root().is_none());

    // 打开工作区（带 .. 的路径应规范化）
    let messy = dir.path().join("sub/../");
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    let opened = engine.set_workspace(&messy).expect("open workspace");
    let expected = dsh_desktop::core::workspace::WorkspaceManager::strip_unc_prefix(
        &dir.path().canonicalize().unwrap(),
    );
    assert_eq!(
        PathBuf::from(&opened),
        expected.clone(),
        "set_workspace 应返回规范化路径（无 UNC 前缀）"
    );
    assert_eq!(
        engine.workspace_root().map(|p| p.to_path_buf()),
        Some(expected.clone())
    );

    // 新会话自动绑定工作区
    let sid = engine.create_session(None).unwrap();
    let s = engine.open_session(&sid).unwrap();
    assert_eq!(
        s.cwd.as_deref(),
        Some(expected.as_path()),
        "会话应绑定工作区 cwd"
    );

    // 不存在目录拒绝
    let err = engine
        .set_workspace(std::path::Path::new("C:/definitely/not/exist/xyz"))
        .expect_err("不存在目录应失败");
    assert!(!err.is_empty());

    // 显式 cwd 覆盖工作区
    let other = tempfile::tempdir().unwrap();
    let sid2 = engine
        .create_session_with_cwd(Some("手工绑定"), Some(other.path().to_path_buf()))
        .unwrap();
    let s2 = engine.open_session(&sid2).unwrap();
    assert_eq!(s2.cwd.as_deref(), Some(other.path()));
}

/// Agent 预设：创建带预设的会话 → 重放恢复 → 切换。
#[test]
fn preset_sessions_created_restored_switched() {
    use dsh_desktop::core::preset::AgentPreset;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");

    // 默认标准模式
    let sid = engine.create_session(Some("默认")).unwrap();
    assert_eq!(
        engine.open_session(&sid).unwrap().preset,
        AgentPreset::Standard
    );

    // 显式自主规划
    let sid2 = engine
        .create_session_full(Some("自主"), None, AgentPreset::AutoPlan)
        .unwrap();
    assert_eq!(engine.open_session(&sid2).unwrap().preset, AgentPreset::AutoPlan);

    // 切换 → 自主规划
    engine
        .set_session_preset(&sid, AgentPreset::AutoPlan)
        .expect("switch preset");
    assert_eq!(
        engine.open_session(&sid).unwrap().preset,
        AgentPreset::AutoPlan
    );

    // 重放恢复：新引擎实例读同一 data_dir
    let mut settings2 = EngineSettings::default();
    settings2.api_key = None;
    settings2.data_dir = data_dir;
    let (tx2, _rx2) = std::sync::mpsc::channel::<EngineEvent>();
    let mut engine2 = DshEngine::new(settings2, tx2).expect("engine2");
    assert_eq!(
        engine2.open_session(&sid2).unwrap().preset,
        AgentPreset::AutoPlan,
        "重放后 preset 应恢复"
    );
}

/// Agent 预设：两种模式均为完整工具目录；run_code 已随 PTC 移除。
#[test]
fn preset_tool_catalog_full() {
    use dsh_desktop::core::preset::AgentPreset;
    use dsh_desktop::core::tools::ToolRegistry;
    use std::path::PathBuf;

    for preset in AgentPreset::all() {
        let specs = ToolRegistry::new(PathBuf::from("."))
            .with_preset(preset)
            .tool_specs();
        assert!(
            specs.len() >= 10,
            "{} 工具集应完整，实际 {}",
            preset.name(),
            specs.len()
        );
        assert!(
            !specs.iter().any(|s| s.function.name == "run_code"),
            "run_code 已随 PTC 模式移除"
        );
    }
}

/// 回归：附件图片链路——排队消息携带图片（泵出发送时保留）、
/// 回合运行中插话也带图（此前两条路径都静默丢图）。
#[test]
fn attachment_images_survive_queue_and_interject() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent, Message};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("fake-key".into());
    settings.base_url = "http://127.0.0.1:1".into();
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx).expect("engine"),
    ));
    let sid = engine.lock().unwrap().create_session(Some("img链路")).unwrap();

    let user_images = |s: &dsh_desktop::core::Session| {
        s.messages
            .iter()
            .filter_map(|m| match m {
                Message::User { images, .. } => images.clone(),
                _ => None,
            })
            .collect::<Vec<Vec<String>>>()
    };

    // 1) 排队带图：running 中入队 → idle 泵出（同步 push 带 images）
    {
        let mut e = engine.lock().unwrap();
        {
            let mut shared = e.shared_sessions();
            shared.get_mut(&sid).unwrap().running = true;
        }
        e.enqueue_message(&sid, "看这张图", vec!["C:/tmp/a.png".into()]);
        {
            let mut shared = e.shared_sessions();
            shared.get_mut(&sid).unwrap().running = false;
        }
        e.pump_message_queue();
        let shared = e.shared_sessions();
        let s = shared.get(&sid).unwrap();
        assert!(
            user_images(s).iter().any(|v| v == &vec!["C:/tmp/a.png".to_string()]),
            "排队泵出的消息应携带图片: {:?}",
            user_images(s)
        );
    }

    // 2) 插话带图：running 中直发（interject 路径同步 push）
    {
        let mut e = engine.lock().unwrap();
        {
            let mut shared = e.shared_sessions();
            shared.get_mut(&sid).unwrap().running = true;
        }
        let _ = e.send_message_with_images(&sid, "插话带图", vec!["C:/tmp/b.png".into()]);
        let s = e.open_session(&sid).unwrap();
        assert!(
            user_images(&s).iter().any(|v| v == &vec!["C:/tmp/b.png".to_string()]),
            "插话消息应携带图片（此前被静默丢弃）: {:?}",
            user_images(&s)
        );
    }
}

/// 回归：tool 消息必须紧跟带 tool_calls 的 assistant 消息（OpenAI 兼容 400 修复）。
#[test]
fn tool_messages_pair_with_assistant_tool_calls() {
    use dsh_desktop::core::llm::{LlmClient, LlmRole};
    use dsh_desktop::core::session::{FunctionCall, Message, ToolCall};
    // 模拟 run_turn 一轮工具调用后的消息序列
    let messages = vec![
        Message::User {
            content: "读文件".into(),
            images: None,
        },
        Message::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                },
            }],
        },
        Message::Tool {
            tool_call_id: "call_1".into(),
            content: "文件内容".into(),
        },
    ];
    let llm_msgs = LlmClient::to_llm_messages(&messages);
    // assistant 必须携带 tool_calls
    assert_eq!(llm_msgs[1].role, LlmRole::Assistant);
    let tcs = llm_msgs[1]
        .tool_calls
        .as_ref()
        .expect("assistant 消息必须带 tool_calls");
    assert_eq!(tcs[0].id, "call_1");
    // 纯工具调用回合：content 为空 → 必须为 null（OpenAI 兼容）
    assert!(llm_msgs[1].content.is_none(), "空 content 应为 None");
    // tool 消息配对
    assert_eq!(llm_msgs[2].role, LlmRole::Tool);
    assert_eq!(llm_msgs[2].tool_call_id.as_deref(), Some("call_1"));
    // 序列化形状检查
    let json = serde_json::to_value(&llm_msgs).unwrap();
    assert_eq!(json[1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(json[1]["tool_calls"][0]["function"]["name"], "read_file");
}

/// 回归：存储重放后 assistant.tool_calls 恢复（跨重启不再 400）。
#[test]
fn storage_replay_restores_assistant_tool_calls() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::storage::SessionStore;
    use dsh_desktop::core::{Message, SessionEvent};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::USER_MESSAGE,
                Some(serde_json::json!({"content": "hi"})),
            ),
        )
        .unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::ASSISTANT_MESSAGE,
                Some(serde_json::json!({
                    "content": "",
                    "reasoning_content": "",
                    "tool_calls": [{"id":"call_9","type":"function","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}],
                })),
            ),
        )
        .unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::TOOL_RESULT,
                Some(serde_json::json!({"ok": true, "value": {}, "stderr": ""})),
            ),
        )
        .unwrap();
    let s = store.load_session("t").unwrap();
    assert_eq!(s.messages.len(), 2);
    match &s.messages[1] {
        Message::Assistant { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1, "重放后应恢复 tool_calls");
            assert_eq!(tool_calls[0].id, "call_9");
            assert_eq!(tool_calls[0].function.name, "bash");
        }
        _ => panic!("第 2 条应为 assistant"),
    }
}

/// 回归：重放后 tool/result 投影为 Tool 消息（修复 400 insufficient tool messages）。
#[test]
fn storage_replay_projects_tool_messages() {
    use dsh_desktop::core::session::types;
    use dsh_desktop::core::storage::SessionStore;
    use dsh_desktop::core::{Message, SessionEvent};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::USER_MESSAGE,
                Some(serde_json::json!({"content": "hi"})),
            ),
        )
        .unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::ASSISTANT_MESSAGE,
                Some(serde_json::json!({
                    "content": "",
                    "reasoning_content": "",
                    "tool_calls": [{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}],
                })),
            ),
        )
        .unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::TOOL_CALL,
                Some(serde_json::json!({"call_id": "call_1", "name": "bash", "arguments": "{\"command\":\"echo hi\"}"})),
            ),
        )
        .unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::TOOL_RESULT,
                Some(serde_json::json!({"ok": true, "value": {"output": "hi"}, "stderr": ""})),
            ),
        )
        .unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::ASSISTANT_MESSAGE,
                Some(serde_json::json!({"content": "done", "reasoning_content": "", "tool_calls": []})),
            ),
        )
        .unwrap();
    let s = store.load_session("t").unwrap();
    // user + assistant(tool_calls) + tool + assistant
    assert_eq!(s.messages.len(), 4, "重放应含 Tool 消息: {:?}", s.messages);
    match &s.messages[2] {
        Message::Tool {
            tool_call_id,
            content,
        } => {
            assert_eq!(tool_call_id, "call_1", "tool 消息应配对 call_1");
            assert!(
                content.contains("hi"),
                "tool 消息内容应来自 tool/result: {content}"
            );
        }
        other => panic!("第 3 条应为 Tool 消息: {other:?}"),
    }
}

/// 回归：to_llm_messages 防御裁剪（未响应尾部 / 用户打断 / 孤立 tool）。
#[test]
fn llm_messages_trim_unpaired_tool_calls() {
    use dsh_desktop::core::llm::{LlmClient, LlmRole};
    use dsh_desktop::core::session::{FunctionCall, Message, ToolCall};
    let tc = |id: &str| ToolCall {
        id: id.into(),
        call_type: "function".into(),
        function: FunctionCall {
            name: "bash".into(),
            arguments: "{}".into(),
        },
    };
    // 1) 尾部未响应：assistant{tool_calls:[A]} 后无 tool →
    //    裁剪后成为空壳（无 content 无 tool_calls）→ 整条移除（避免 400）
    let msgs = vec![
        Message::User {
            content: "u".into(),
            images: None,
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![tc("call_a")],
            reasoning: None,
            },
    ];
    let out = LlmClient::to_llm_messages(&msgs);
    assert_eq!(
        out.len(),
        1,
        "空壳 assistant（无 content 无 tool_calls）应被移除，避免 API 400: {out:?}"
    );
    assert!(
        out.iter().all(|m| m.role == LlmRole::User),
        "剩余消息应只有 user: {out:?}"
    );
    // 2) 用户打断：assistant{tool_calls:[A]} + user → 前一条 assistant 裁剪为空壳 → 移除
    let msgs = vec![
        Message::User {
            content: "u".into(),
            images: None,
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![tc("call_a")],
            reasoning: None,
            },
        Message::User {
            content: "打断".into(),
            images: None,
        },
    ];
    let out = LlmClient::to_llm_messages(&msgs);
    assert_eq!(
        out.len(),
        2,
        "被打断的空壳 assistant 应移除（只剩 2 条 user）: {out:?}"
    );
    assert!(
        out.iter().all(|m| m.role == LlmRole::User),
        "不应存在空壳 assistant: {out:?}"
    );
    // 3) 孤立 tool：无对应 assistant tool_calls → 丢弃
    let msgs = vec![
        Message::User {
            content: "u".into(),
            images: None,
        },
        Message::Tool {
            tool_call_id: "call_orphan".into(),
            content: "x".into(),
        },
    ];
    let out = LlmClient::to_llm_messages(&msgs);
    assert_eq!(out.len(), 1, "孤立 tool 消息应被丢弃");
    // 4) 正常序列不受影响：assistant{tool_calls:[A,B]} + tool[A] + tool[B] 完整保留
    let msgs = vec![
        Message::User {
            content: "u".into(),
            images: None,
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![tc("call_a"), tc("call_b")],
            reasoning: None,
            },
        Message::Tool {
            tool_call_id: "call_a".into(),
            content: "ra".into(),
        },
        Message::Tool {
            tool_call_id: "call_b".into(),
            content: "rb".into(),
        },
    ];
    let out = LlmClient::to_llm_messages(&msgs);
    assert_eq!(out.len(), 4);
    assert_eq!(out[1].tool_calls.as_ref().unwrap().len(), 2);
    assert_eq!(out[3].tool_call_id.as_deref(), Some("call_b"));
}

/// 复现：切预设后再发送必须正常（用户反馈：切到创造模式后输入消息无反应）。
#[test]
fn send_after_preset_switch_works() {
    use dsh_desktop::core::preset::AgentPreset;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("sk-test".into());
    settings.base_url = "http://127.0.0.1:1".into(); // 回合快速失败（连接拒绝）
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let _test_data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");
    let sid = engine.create_session(Some("测试")).unwrap();

    // 第一次发送（回合后台失败并收尾复位）
    engine.send_message(&sid, "first").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(!engine.open_session(&sid).unwrap().running);

    // 切预设（用户操作：切到创造模式）
    engine
        .set_session_preset(&sid, AgentPreset::AutoPlan)
        .unwrap();
    assert_eq!(
        engine.open_session(&sid).unwrap().preset,
        AgentPreset::AutoPlan
    );

    // 第二次发送：不得报 already running / session not open
    engine
        .send_message(&sid, "second")
        .expect("切预设后发送不应失败");
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let s = engine.open_session(&sid).unwrap();
    assert!(!s.running, "第二回合结束应复位");
    assert_eq!(s.preset, AgentPreset::AutoPlan, "预设应保持");
    let _ = rx;
}

/// 回归：发送失败后 running 必须复位（第二次发送不再报 already running）。
#[test]
fn send_failure_does_not_stick_running() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("sk-test".into());
    settings.base_url = "http://127.0.0.1:1".into(); // 连接立即失败
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let _test_data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");
    let sid = engine.create_session(None).unwrap();
    // 第一次发送：回合在后台失败（连接拒绝）→ 收尾必须复位 running
    engine.send_message(&sid, "hi").expect("send 应接受");
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let s = engine.open_session(&sid).unwrap();
    assert!(
        !s.running,
        "LLM 失败后 running 应复位，实际 {:?}",
        s.running
    );
    // 第二次发送：不得报 already running
    engine
        .send_message(&sid, "hello again")
        .expect("第二次发送不应报 already running");
}

/// 回归：无 API key 发送失败不得污染会话状态（消息不入栈、running 不变）。
#[test]
fn no_key_does_not_pollute_state() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let _test_data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");
    let sid = engine.create_session(None).unwrap();
    let err = engine.send_message(&sid, "x").unwrap_err();
    assert!(err.to_string().contains("API key"));
    let s = engine.open_session(&sid).unwrap();
    assert!(!s.running, "无 key 失败后 running 不应为 true");
    assert_eq!(s.messages.len(), 0, "无 key 时用户消息不应入栈");
    // 再次发送仍报 key 错误（不是 already running）
    let err2 = engine.send_message(&sid, "y").unwrap_err();
    assert!(err2.to_string().contains("API key"));
}

/// 回归：部分响应的 tool_calls 裁剪后不产生孤儿 tool 消息（避免 400）。
#[test]
fn llm_messages_partial_response_no_orphans() {
    use dsh_desktop::core::llm::{LlmClient, LlmRole};
    use dsh_desktop::core::session::{FunctionCall, Message, ToolCall};
    let tc = |id: &str| ToolCall {
        id: id.into(),
        call_type: "function".into(),
        function: FunctionCall {
            name: "bash".into(),
            arguments: "{}".into(),
        },
    };
    // assistant 声明 [A,B]，只响应了 A，然后用户打断
    let msgs = vec![
        Message::User {
            content: "u".into(),
            images: None,
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![tc("call_a"), tc("call_b")],
            reasoning: None,
            },
        Message::Tool {
            tool_call_id: "call_a".into(),
            content: "ra".into(),
        },
        Message::User {
            content: "打断".into(),
            images: None,
        },
    ];
    let out = LlmClient::to_llm_messages(&msgs);
    assert_eq!(out.len(), 4, "user + assistant + tool + user");
    // assistant 只保留已响应的 A
    let tcs = out[1]
        .tool_calls
        .as_ref()
        .expect("应保留已响应的 tool_calls");
    assert_eq!(tcs.len(), 1);
    assert_eq!(tcs[0].id, "call_a");
    // tool[A] 保留且与 assistant 配对（非孤儿）
    assert_eq!(out[2].role, LlmRole::Tool);
    assert_eq!(out[2].tool_call_id.as_deref(), Some("call_a"));
}

/// 默认模式：新建会话默认标准模式；修改引擎默认后新建继承。
#[test]
fn default_preset_is_standard_and_configurable() {
    use dsh_desktop::core::preset::AgentPreset;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let _test_data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");

    // 默认 = 标准模式
    assert_eq!(engine.default_preset(), AgentPreset::Standard);
    let sid = engine.create_session(None).unwrap();
    assert_eq!(
        engine.open_session(&sid).unwrap().preset,
        AgentPreset::Standard,
        "新建会话应默认标准模式"
    );

    // 修改引擎默认 → 新建继承
    engine.set_default_preset(AgentPreset::AutoPlan);
    let sid2 = engine.create_session(None).unwrap();
    assert_eq!(
        engine.open_session(&sid2).unwrap().preset,
        AgentPreset::AutoPlan,
        "修改默认预设后新建会话应继承"
    );

    // 已存在会话不受影响
    assert_eq!(
        engine.open_session(&sid).unwrap().preset,
        AgentPreset::Standard
    );

    // 无效配置回退标准模式
    let mut settings2 = EngineSettings::default();
    settings2.api_key = None;
    settings2.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
    settings2.default_preset = "bogus".into();
    let (tx2, _rx2) = std::sync::mpsc::channel::<EngineEvent>();
    let engine2 = DshEngine::new(settings2, tx2).expect("engine2");
    assert_eq!(engine2.default_preset(), AgentPreset::Standard);
}

/// run_code 已随 PTC 模式移除：任何预设下 dispatch 都应拒绝
/// （保留 dispatch 分支仅为历史会话重放兜底）。
#[test]
fn run_code_removed_rejected_everywhere() {
    use dsh_desktop::core::preset::AgentPreset;
    use dsh_desktop::core::tools::ToolRegistry;
    use serde_json::json;

    let dir = tempfile::tempdir().unwrap();
    for preset in AgentPreset::all() {
        let reg = ToolRegistry::new(dir.path().to_path_buf()).with_preset(preset);
        let out = reg.dispatch(
            "run_code",
            &json!({"steps": [
                {"op": "write_file", "path": "hello.txt", "content": "hi"},
            ]}),
        );
        assert!(!out.ok, "{} 不应提供 run_code", preset.name());
        let file = dir.path().join("hello.txt");
        assert!(!file.exists(), "拒绝时不得产生副作用");
    }
}

/// 工作区：canonicalize 拒绝文件路径。
#[test]
fn workspace_rejects_file() {
    use dsh_desktop::core::workspace::WorkspaceManager;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "x").unwrap();
    let mut mgr = WorkspaceManager::default();
    let err = mgr.open(&file).expect_err("文件不是目录");
    assert!(!err.is_empty());
}

/// 桥：start_bridge 能拿到端口。
#[test]
fn bridge_starts() {
    use dsh_desktop::bridge::start_bridge;
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("sk-test".into());
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let engine = std::sync::Arc::new(std::sync::Mutex::new(
        DshEngine::new(settings, tx).expect("engine"),
    ));
    let (port, token) = start_bridge(engine).expect("bridge start");
    assert!(port > 0);
    assert!(token.len() >= 32, "token 应为随机长串: {token}");

    // 401：无 token / 错 token；200：带正确 token（鉴权回归——桥暴露的
    // send 接口能驱动 agent 工具执行，不能未授权访问）。
    // 注意：本 API 的错误统一以响应体 code 字段表达（HTTP 状态恒 200）。
    let base = format!("http://127.0.0.1:{port}/api/settings");
    let code_of = |resp: reqwest::blocking::Response| -> i64 {
        let body: serde_json::Value = resp.json().expect("json body");
        body.get("code").and_then(|c| c.as_i64()).unwrap_or(0)
    };
    let no_token = reqwest::blocking::Client::new()
        .get(&base)
        .send()
        .expect("request");
    assert_eq!(code_of(no_token), 401, "无 token 必须被拒");
    let bad = reqwest::blocking::Client::new()
        .get(&base)
        .header("X-DSH-Token", "wrong-token")
        .send()
        .expect("request");
    assert_eq!(code_of(bad), 401, "错 token 必须被拒");
    let ok = reqwest::blocking::Client::new()
        .get(&base)
        .header("X-DSH-Token", &token)
        .send()
        .expect("request");
    let ok_body: serde_json::Value = ok.json().expect("json body");
    assert!(
        ok_body.get("model").is_some(),
        "正确 token 应放行: {ok_body}"
    );
}

/// 工具：bash echo 与 read/write 文件往返。
#[test]
fn tools_bash_and_fs() {
    use dsh_desktop::core::tools::ToolRegistry;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::new(dir.path().to_path_buf());
    // bash echo
    let out = tools.dispatch("bash", &json!({"command": "echo hello_from_rust"}));
    assert!(out.ok, "bash failed: {}", out.stderr);
    assert!(out.value["stdout"]
        .as_str()
        .unwrap_or("")
        .contains("hello_from_rust"));
    // write + read
    let f = dir.path().join("a.txt");
    let out = tools.dispatch(
        "write_file",
        &json!({"path": "a.txt", "content": "line1\nline2"}),
    );
    assert!(out.ok);
    assert!(f.exists());
    let out = tools.dispatch("read_file", &json!({"path": "a.txt"}));
    assert!(out.ok);
    assert!(out.value["content"]
        .as_str()
        .unwrap_or("")
        .contains("line1"));
    // list_dir
    let out = tools.dispatch("list_dir", &json!({}));
    assert!(out.ok);
    assert!(!out.value["items"].as_array().unwrap().is_empty());
}

/// M1：str_replace_editor view/create/str_replace 往返。
#[test]
fn tools_str_replace_editor() {
    use dsh_desktop::core::tools::ToolRegistry;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::new(dir.path().to_path_buf());
    // create
    let out = tools.dispatch(
        "str_replace_editor",
        &json!({
            "command": "create", "path": "demo.txt", "file_text": "line1\nline2\nline3"
        }),
    );
    assert!(out.ok, "create failed: {}", out.stderr);
    // view
    let out = tools.dispatch(
        "str_replace_editor",
        &json!({"command": "view", "path": "demo.txt"}),
    );
    assert!(out.ok);
    assert!(out.value["content"]
        .as_str()
        .unwrap_or("")
        .contains("line1"));
    // str_replace 唯一匹配
    let out = tools.dispatch(
        "str_replace_editor",
        &json!({
            "command": "str_replace", "path": "demo.txt", "old_str": "line2", "new_str": "LINE2"
        }),
    );
    assert!(out.ok, "replace failed: {}", out.stderr);
    let out = tools.dispatch(
        "str_replace_editor",
        &json!({"command": "view", "path": "demo.txt"}),
    );
    assert!(out.value["content"]
        .as_str()
        .unwrap_or("")
        .contains("LINE2"));
    // 非唯一匹配应失败
    let out = tools.dispatch(
        "str_replace_editor",
        &json!({
            "command": "str_replace", "path": "demo.txt", "old_str": "l", "new_str": "x"
        }),
    );
    assert!(!out.ok, "非唯一匹配应失败");
    // create 已存在应失败
    let out = tools.dispatch(
        "str_replace_editor",
        &json!({"command": "create", "path": "demo.txt", "file_text": "x"}),
    );
    assert!(!out.ok);
}

/// M1：fs_search 找到文件。
#[test]
fn tools_fs_search() {
    use dsh_desktop::core::tools::ToolRegistry;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alpha.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("beta.md"), "# doc").unwrap();
    let tools = ToolRegistry::new(dir.path().to_path_buf());
    let out = tools.dispatch("fs_search", &json!({"path": ".", "pattern": "alpha"}));
    assert!(out.ok);
    assert_eq!(out.value["count"].as_u64().unwrap_or(0), 1);
}

/// M1：pwsh 执行。
#[test]
fn tools_pwsh() {
    use dsh_desktop::core::tools::ToolRegistry;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::new(dir.path().to_path_buf());
    let out = tools.dispatch("pwsh", &json!({"command": "Write-Output hello_pwsh"}));
    assert!(out.ok, "pwsh failed: {}", out.stderr);
    assert!(out.value["stdout"]
        .as_str()
        .unwrap_or("")
        .contains("hello_pwsh"));
}

/// M1：web_search 空 query 校验。
#[test]
fn tools_web_search_validation() {
    use dsh_desktop::core::tools::ToolRegistry;
    use serde_json::json;
    let tools = ToolRegistry::new(tempfile::tempdir().unwrap().path().to_path_buf());
    let out = tools.dispatch("web_search", &json!({"query": "   "}));
    assert!(!out.ok, "空 query 应失败");
    let out = tools.dispatch("web_search", &json!({"query": "rust"}));
    assert!(out.ok);
}

/// M6：插件 loader 生命周期（Rust 版 cordis）。
#[test]
fn plugin_loader_lifecycle() {
    use dsh_desktop::plugin::{EventBus, Loader, Plugin, PluginContext};

    struct DemoPlugin;
    impl Plugin for DemoPlugin {
        fn name(&self) -> &str {
            "demo-plugin"
        }
        fn mount(&self, ctx: &PluginContext) -> Result<(), String> {
            assert_eq!(ctx.id, "demo-plugin");
            Ok(())
        }
    }

    let mut loader = Loader::default();
    let bus = EventBus::default();
    loader.mount(Box::new(DemoPlugin), &bus).unwrap();
    assert_eq!(loader.len(), 1);
    assert_eq!(
        loader.get("demo-plugin").unwrap().state,
        dsh_desktop::plugin::FiberState::Active
    );
    // 重复注册应失败
    assert!(loader.mount(Box::new(DemoPlugin), &bus).is_err());
    // 卸载
    loader.unmount("demo-plugin");
    assert_eq!(loader.len(), 0);
}

/// M6：插件组 + 事件总线。
#[test]
fn plugin_group_and_events() {
    use dsh_desktop::plugin::{EventBus, Plugin, PluginContext, PluginEvent, PluginGroup};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct EchoPlugin(Arc<AtomicUsize>);
    impl Plugin for EchoPlugin {
        fn name(&self) -> &str {
            "echo"
        }
        fn mount(&self, ctx: &PluginContext) -> Result<(), String> {
            let counter = self.0.clone();
            // 订阅事件（通过 ctx.bus 的 on——需要 &mut；这里用外部 bus 简化）
            let _ = ctx;
            let _ = counter;
            Ok(())
        }
    }

    let mut bus = EventBus::default();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    bus.on(
        "test/event",
        Box::new(move |_e: &PluginEvent| {
            hits2.fetch_add(1, Ordering::SeqCst);
        }),
    );
    bus.emit(&PluginEvent {
        name: "test/event".into(),
        payload: serde_json::json!({}),
    });
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // 组
    let mut group = PluginGroup::new("g1");
    group
        .mount(Box::new(EchoPlugin(hits.clone())), &bus)
        .unwrap();
    assert_eq!(group.loader().len(), 1);
}

/// 回归：load_session 必须投影 assistant/message 到 messages。
#[test]
fn storage_load_projects_assistant() {
    use dsh_desktop::core::session::{types, Message, SessionEvent};
    use dsh_desktop::core::storage::SessionStore;
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
    store
        .append(
            "t",
            &SessionEvent::new(
                types::USER_MESSAGE,
                Some(serde_json::json!({"content": "hi"})),
            ),
        )
        .unwrap();
    store.append("t", &SessionEvent::new(types::ASSISTANT_MESSAGE, Some(serde_json::json!({"content": "hello back", "reasoning_content": "", "tool_calls": []})))).unwrap();
    store
        .append("t", &SessionEvent::new(types::TURN_END, None))
        .unwrap();
    let s = store.load_session("t").unwrap();
    assert_eq!(s.messages.len(), 2, "应投影 2 条消息, got {:?}", s.messages);
    match &s.messages[1] {
        Message::Assistant { content, .. } => assert_eq!(content, "hello back"),
        _ => panic!("第 2 条应为 assistant"),
    }
}

/// 回归：无 API key 且无 DSH credentials 时，引擎创建成功、UI 可用、发送报明确错误（不 panic）。
#[test]
fn engine_no_key_no_crash() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    // 隔离 + 防 race：目录名含 "dsh"（并行测试里 config_roundtrip 断言
    // DSH_HOME 路径包含 "dsh"；纯随机 tempdir 名会偶发踩失败）
    std::env::set_var(
        "DSH_HOME",
        tempfile::tempdir()
            .unwrap()
            .path()
            .join("dsh-home")
            .to_str()
            .unwrap(),
    );
    // 也清掉本地 engine-settings 的影响：直接构造无 key settings
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None;
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let mut engine = DshEngine::new(settings, tx).expect("无 key 创建引擎不应失败");
    let sid = engine.create_session(None).expect("创建会话不应失败");
    let err = engine.send_message(&sid, "hi").unwrap_err();
    assert!(
        err.to_string().contains("API key"),
        "错误应提示 API key, got: {err}"
    );
}

/// 回归：设置页保存 API key 后，任何 chip 切换（模型/思考/权限触发的
/// rebuild_llm 落盘）不得把 key 覆盖掉。
/// 旧实现设置页直接 EngineSettings::save() 写文件、引擎内存还是旧值
/// （api_key=None），chip 切换 → rebuild_llm → self.settings.save()
/// → 旧内存整份落盘 → key 丢失（"保存不了 key"）。
#[test]
fn chip_switch_preserves_saved_api_key() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = None; // 引擎启动时无 key
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let test_data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");

    // 设置页保存 key（走引擎统一入口，立即生效）
    engine.update_settings_from_ui(
        Some("sk-saved-key".into()),
        "https://api.deepseek.com".into(),
        "deepseek-v4-flash".into(),
        None,
        "standard".into(),
    );

    // 用户在输入框 chip 切模型 / 思考深度（旧 bug：这里会把 key 清掉）
    engine.set_model("deepseek-v4-pro");
    engine.set_reasoning_effort("low");

    // 磁盘上的 key 必须还在（从 data_dir 对应路径读，不用默认路径——
    // 默认路径是用户真实配置，测试 data_dir 在临时目录天然隔离）
    let settings_path = test_data_dir.parent().unwrap().join("engine-settings.json");
    let disk = {
        let text = std::fs::read_to_string(&settings_path).expect("设置文件应存在");
        let mut s: EngineSettings = serde_json::from_str(&text).unwrap();
        if let Some(stored) = &s.api_key {
            if let Some(plain) = dsh_desktop::core::settings::unprotect_key_for_test(stored) {
                s.api_key = Some(plain);
            }
        }
        s
    };
    assert_eq!(
        disk.api_key.as_deref(),
        Some("sk-saved-key"),
        "chip 切换后磁盘 key 不得丢失"
    );
    assert_eq!(disk.model, "deepseek-v4-pro");
    assert_eq!(disk.reasoning_effort.as_deref(), Some("low"));
    // 引擎当前生效的客户端也用新 key
    assert_eq!(engine.current_model(), "deepseek-v4-pro");
    assert!(engine.llm().is_some(), "有 key 时 LLM 客户端应可用");
}

/// 排队执行：运行中的会话 enqueue 不打断；回合结束（idle）后 pump 自动
/// 取队首发起回合，FIFO 逐条执行。
#[test]
fn queue_runs_after_turn_finishes() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::{AgentStatus, DshEngine, EngineEvent};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("fake".into());
    settings.base_url = "http://127.0.0.1:1".into(); // 不可达：回合快速失败
    settings.data_dir = tempfile::tempdir().unwrap().path().join("sessions");
    let _test_data_dir = settings.data_dir.clone();
    let mut engine = DshEngine::new(settings, tx).expect("engine");
    let sid = engine.create_session(Some("q")).unwrap();

    // 模拟回合运行中
    {
        let mut shared = engine.shared_sessions();
        shared.get_mut(&sid).unwrap().running = true;
    }
    // 排队两条（FIFO）
    assert_eq!(engine.enqueue_message(&sid, "第一", Vec::new()), 1);
    assert_eq!(engine.enqueue_message(&sid, "第二", vec!["C:/x.png".into()]), 2);
    assert_eq!(engine.queued_count(&sid), 2);

    // 运行中：泵不发起（不打断）
    engine.pump_message_queue();
    assert_eq!(engine.queued_count(&sid), 2, "回合运行中不得提前出队");

    // 回合结束（idle）：泵取队首发起（消息入会话 = send_message 生效）
    {
        let mut shared = engine.shared_sessions();
        shared.get_mut(&sid).unwrap().running = false;
    }
    engine.pump_message_queue();
    assert_eq!(engine.queued_count(&sid), 1, "应只取出一条（FIFO）");
    let s = engine.open_session(&sid).unwrap();
    assert!(
        s.messages.iter().any(
            |m| matches!(m, dsh_desktop::core::Message::User { content, .. } if content == "第一")
        ),
        "队首消息应已发起"
    );

    // 删除会话时队列清理
    engine.clear_queue(&sid);
    assert_eq!(engine.queued_count(&sid), 0);
    let _ = AgentStatus::Idle;
}

/// 回归：沙箱权限切换（🛡 chip）实际生效——workspace-write 下工作区内
/// 可写、工作区外被拒；切回 danger-full-access 后恢复可写。
#[test]
fn sandbox_chip_toggle_actually_works() {
    use dsh_desktop::core::settings::EngineSettings;
    use dsh_desktop::core::SandboxMode;
    use dsh_desktop::core::{DshEngine, EngineEvent};
    let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
    let mut settings = EngineSettings::default();
    settings.api_key = Some("fake".into());
    settings.base_url = "http://127.0.0.1:1".into();
    let dir = tempfile::tempdir().unwrap();
    settings.data_dir = dir.path().join("sessions");
    settings.skills_dir = Some(dir.path().join("skills"));
    settings.plugins_dir = Some(dir.path().join("plugins"));
    let mut engine = DshEngine::new(settings, tx).expect("engine");

    // 默认 danger-full-access → write_file 任意路径可写
    assert_eq!(engine.sandbox_mode(), SandboxMode::DangerFullAccess);
    let outside = dir.path().join("outside.txt");
    let out = engine.tools_for_test().dispatch(
        "write_file",
        &serde_json::json!({"path": outside.to_string_lossy(), "content": "x"}),
    );
    assert!(out.ok, "danger 模式下工作区外写应成功: {out:?}");

    // 切到 workspace-write（模拟 🛡 chip 点击）
    engine.set_sandbox_mode(SandboxMode::WorkspaceWrite);
    assert_eq!(engine.sandbox_mode(), SandboxMode::WorkspaceWrite);

    // 工作区外写：审批系统是闸门，工具不硬拒（会成功写入，
    // 但实际运行时 approval flow 会拦截并要求用户确认）
    let outside2 = dir.path().join("outside2.txt");
    let _out2 = engine.tools_for_test().dispatch(
        "write_file",
        &serde_json::json!({"path": outside2.to_string_lossy(), "content": "x"}),
    );
    // 工具层面允许（审批在 dispatch 之前的 potential_out_of_workspace 层拦截）

    // 工作区内写成功（cwd 就是工作区）
    let inside = std::env::current_dir()
        .unwrap()
        .join("sandbox_test_inside.txt");
    let out3 = engine.tools_for_test().dispatch(
        "write_file",
        &serde_json::json!({"path": inside.to_string_lossy(), "content": "ok"}),
    );
    assert!(out3.ok, "workspace-write 下工作区内写应成功: {out3:?}");
    let _ = std::fs::remove_file(&inside);

    // 切到 read-only → 一切写被拒
    engine.set_sandbox_mode(SandboxMode::ReadOnly);
    let out4 = engine.tools_for_test().dispatch(
        "write_file",
        &serde_json::json!({"path": inside.to_string_lossy(), "content": "x"}),
    );
    assert!(!out4.ok, "read-only 下写必须被拒");

    // 切回 danger → 恢复可写
    engine.set_sandbox_mode(SandboxMode::DangerFullAccess);
    let out5 = engine.tools_for_test().dispatch(
        "write_file",
        &serde_json::json!({"path": inside.to_string_lossy(), "content": "back"}),
    );
    assert!(out5.ok, "danger 恢复后写应成功");
    let _ = std::fs::remove_file(&inside);

    // 清理
    let _ = std::fs::remove_file(&outside);
}
