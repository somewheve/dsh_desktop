import io

p = r'E:\AI\dsh_desktop\src\ui\chat_tab.rs'
s = io.open(p, encoding='utf-8').read()

# ===== 1. 空态引导卡片 =====
old = """if session.messages.is_empty() && !has_stream {
                        ui.add_space(20.0);
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                RichText::new(tr(
                                    self.lang,
                                    "开始对话 —— 输入你的问题，AI 的思考会飘过",
                                    "Start chatting — type your question; the AI's thinking floats by",
                                ))
                                .color(Theme::text_faint()),
                            );
                        });
                    }"""

START = chr(0x5F00) + chr(0x59CB) + chr(0x5BF9) + chr(0x8BDD)  # 开始对话
EMPT_TXT_ZH = START + " —— " + chr(0x8F93) + chr(0x5165) + chr(0x4F60) + chr(0x7684) + chr(0x95EE) + chr(0x9898)

new = """if session.messages.is_empty() && !has_stream {
                        ui.add_space(16.0);
                        let lang = self.lang;
                        let starters: Vec<(&str, String, String)> = vec![
                            ("\U0001F4DD", tr(lang, "\u5199\u6587\u6863", "Write doc"), tr(lang, "\u5e2e\u6211\u5199\u62a5\u544a/\u5468\u62a5/\u6280\u672f\u6587\u6863", "Help me write a report")),
                            ("\U0001F4C0", tr(lang, "\u5199\u4ee3\u7801", "Write code"), tr(lang, "\u5199\u811a\u672c\u89e3\u51b3\u5b9e\u9645\u95ee\u9898", "Write a script")),
                            ("\U0001F50D", tr(lang, "\u641c\u7d22\u8d44\u6599", "Search"), tr(lang, "\u8054\u7f51\u641c\u7d22\u6700\u65b0\u4fe1\u606f", "Search latest info")),
                            ("\U0001F4DA", tr(lang, "\u505a\u8ba1\u5212", "Plan"), tr(lang, "\u5236\u5b9a\u5b66\u4e60/\u5de5\u4f5c/\u65c5\u884c\u8ba1\u5212", "Make a plan")),
                        ];
                        let avail_w = ui.available_width().max(200.0);
                        let card_w = ((avail_w - 40.0) / 2.0).min(220.0).max(140.0);
                        let mut clicked: Option<String> = None;
                        for pair in starters.chunks(2) {
                            ui.horizontal(|ui| {
                                ui.add_space(8.0);
                                for (icon, title, desc) in pair {
                                    let card = egui::Frame::default()
                                        .fill(Theme::bg_elevated())
                                        .stroke(egui::Stroke::new(1.0, Theme::border()))
                                        .corner_radius(egui::CornerRadius::same(10))
                                        .inner_margin(egui::Margin::same(12));
                                    let resp = card.show(ui, |ui| {
                                        ui.set_min_size(egui::vec2(card_w, 68.0));
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(*icon).size(20.0));
                                            ui.label(RichText::new(title.as_str()).size(13.0).strong().color(Theme::text()));
                                        });
                                        ui.add_space(2.0);
                                        ui.label(RichText::new(desc.as_str()).size(10.5).color(Theme::text_faint()));
                                    }).response;
                                    if resp.clicked() {
                                        clicked = Some(desc.clone());
                                    }
                                    ui.add_space(12.0);
                                }
                            });
                            ui.add_space(8.0);
                        }
                        if let Some(prompt) = clicked {
                            self.input = prompt;
                        }
                    }"""

assert old in s, 'empty state not found'
s = s.replace(old, new, 1)
print('1. starter cards: done')

# ===== 2. session_search 字段 =====
s = s.replace(
    '    send_mode: SendMode,',
    '    send_mode: SendMode,\n    /// 会话搜索输入\n    session_search: String,')
s = s.replace(
    '            send_mode: SendMode::Interject,',
    '            send_mode: SendMode::Interject,\n            session_search: String::new(),')
print('2. search field: done')

# ===== 3. 搜索框 + 过滤 =====
old_list = '        ui.add_space(2.0);\n        let sessions = self.sessions.clone();'
new_list = '''        ui.add_space(2.0);
        let search_lower = self.session_search.to_lowercase();
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|s| {
                search_lower.is_empty()
                    || s.title.to_lowercase().contains(&search_lower)
                    || s.session_id.contains(&search_lower)
            })
            .cloned()
            .collect();
        if !search_lower.is_empty() && sessions.is_empty() {
            ui.label(
                egui::RichText::new(tr(lang, "无匹配会话", "No matching sessions"))
                    .size(10.5)
                    .color(Theme::text_faint()),
            );
            return None;
        }'''
assert old_list in s
s = s.replace(old_list, new_list, 1)
print('3. filter: done')

io.open(p, 'w', encoding='utf-8', newline='').write(s)
