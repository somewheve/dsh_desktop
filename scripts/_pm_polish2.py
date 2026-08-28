import io

p = r'E:\AI\dsh_desktop\src\ui\chat_tab.rs'
s = io.open(p, encoding='utf-8').read()

# ============================================================
# 1. 空状态 → 引导卡片
# ============================================================
# 找到 "开始对话" 空态块并替换为引导卡片
old_empty_block = '''if session.messages.is_empty() && !has_stream {
                        ui.add_space(20.0);
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                RichText::new(tr(
                                    self.lang,
                                    "\u{5F00}\u{59CB}\u{5BF9}\u{8BDD} \u{2014}\u{2014} \u{8F93}\u{5165}\u{4F60}\u{7684}\u{95EE}\u{9898}\u{FF0C}AI \u{7684}\u{601D}\u{8003}\u{4F1A}\u{98D8}\u{8FC7}",
                                    "Start chatting \u{2014} type your question; the AI's thinking floats by",
                                ))
                                .color(Theme::TEXT_FAINT),
                            );
                        });
                    }'''

new_empty_block = '''if session.messages.is_empty() && !has_stream {
                        ui.add_space(16.0);
                        // \u{5F15}\u{5BFC}\u{5361}\u{7247}\u{FF1A}\u{7ED9}\u{65B0}\u{7528}\u{6237}\u{65B9}\u{5411}\u{611F}\u{FF08}\u{5BF9}\u{9F50} ChatGPT/Claude \u{7684}\u{7A7A}\u{6001}\u{5F15}\u{5BFC}\u{FF09}
                        let lang = self.lang;
                        let starters: Vec<(&str, &str, &str)> = vec![
                            ("\u{1F4DD}", tr(lang, "\u{5199}\u{4E00}\u{4EFD}\u{6587}\u{6863}", "Write a doc"), tr(lang, "\u{5E2E}\u{6211}\u{5199}\u{4E00}\u{4EFD}\u{5DE5}\u{4F5C}\u{62A5}\u{544A}/\u{5468}\u{62A5}/\u{6280}\u{672F}\u{6587}\u{6863}", "Help me write a report")),
                            ("\u{1F4C0}", tr(lang, "\u{5199}\u{4EE3}\u{7801}", "Write code"), tr(lang, "\u{5199}\u{4E00}\u{4E2A} Python/Rust \u{811A}\u{672C}\u{89E3}\u{51B3}\u{5B9E}\u{9645}\u{95EE}\u{9898}", "Write a script")),
                            ("\u{1F50D}", tr(lang, "\u{641C}\u{7D22}\u{8D44}\u{6599}", "Search"), tr(lang, "\u{8054}\u{7F51}\u{641C}\u{7D22}\u{6700}\u{65B0}\u{4FE1}\u{606F}/\u{653F}\u{7B56}/\u{6280}\u{672F}", "Search latest info")),
                            ("\u{1F4DA}", tr(lang, "\u{5B66}\u{4E60}/\u{8BA1}\u{5212}", "Plan"), tr(lang, "\u{5236}\u{5B9A}\u{5B66}\u{4E60}/\u{5DE5}\u{4F5C}/\u{65C5}\u{884C}\u{8BA1}\u{5212}", "Make a plan")),
                        ];
                        let cols = 2;
                        let rows = (starters.len() + cols - 1) / cols;
                        let avail_w = ui.available_width().max(200.0);
                        let card_w = ((avail_w - 12.0 * (cols as f32 - 1.0) - 16.0) / cols as f32).min(220.0);
                        let card_h = 72.0;
                        let mut clicked_starter: Option<String> = None;
                        for row in 0..rows {
                            ui.horizontal(|ui| {
                                ui.add_space(8.0);
                                for col in 0..cols {
                                    let idx = row * cols + col;
                                    if idx >= starters.len() { break; }
                                    let (icon, title, desc) = &starters[idx];
                                    let card = egui::Frame::default()
                                        .fill(Theme::BG_ELEVATED)
                                        .stroke(egui::Stroke::new(1.0, Theme::BORDER))
                                        .corner_radius(egui::CornerRadius::same(10))
                                        .inner_margin(egui::Margin::same(12));
                                    let resp = card.show(ui, |ui| {
                                        ui.set_min_size(egui::vec2(card_w, card_h));
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(*icon).size(20.0));
                                            ui.label(RichText::new(title.as_str()).size(13.0).strong().color(Theme::TEXT));
                                        });
                                        ui.add_space(2.0);
                                        ui.label(
                                            RichText::new(desc.as_str())
                                                .size(10.5)
                                                .color(Theme::TEXT_FAINT),
                                        );
                                    }).response;
                                    if resp.clicked() {
                                        clicked_starter = Some(desc.clone());
                                    }
                                    if col < cols - 1 {
                                        ui.add_space(12.0);
                                    }
                                }
                            });
                            ui.add_space(8.0);
                        }
                        if let Some(prompt) = clicked_starter {
                            self.input = prompt;
                        }
                    }'''

if old_empty_block in s:
    s = s.replace(old_empty_block, new_empty_block, 1)
    print('1. empty state starter cards: done')
else:
    print('1. empty state: NOT FOUND (may differ after fmt)')

# ============================================================
# 2. AI 消息复制按钮（hover 时右下角出现 📋）
# ============================================================
# 在 render_message 的 Assistant 分支末尾加

# ============================================================
# 3. 会话搜索（ui_session_list 顶部）
# ============================================================
# 加 session_search: String 字段

s = s.replace(
    '''    /// \u{53D1}\u{9001}\u{6A21}\u{5F0F}\u{FF08}\u{63D2}\u{8BDD} / \u{6392}\u{961F}\u{FF09}
    send_mode: SendMode,''',
    '''    /// \u{53D1}\u{9001}\u{6A21}\u{5F0F}\u{FF08}\u{63D2}\u{8BDD} / \u{6392}\u{961F}\u{FF09
    send_mode: SendMode,
    /// \u{4F1A}\u{8BDD}\u{641C}\u{7D22}\u{8F93}\u{5165}\u{6846}\u{5185}\u{5BB9}
    session_search: String,''')

s = s.replace(
    '''            send_mode: SendMode::Interject,''',
    '''            send_mode: SendMode::Interject,
            session_search: String::new(),''')

io.open(p, 'w', encoding='utf-8', newline='').write(s)
print('3. session search field: done')
