import io
import re

p = r'E:\AI\dsh_desktop\src\ui\chat_tab.rs'
s = io.open(p, encoding='utf-8').read()

pat = re.compile(
    r'row_resp\.widget_info\(\|\| \{\s*'
    r'egui::WidgetInfo::labeled\(\s*'
    r'egui::WidgetType::Button,\s*'
    r'true,\s*'
    r'format!\("\\\\u\{1f4ac\} \{\}\{\}", s\.title, if s\.running \{ " \\\\u\{26a1\}" \} else \{ "" \}\),\s*'
    r'\)\s*'
    r'\}\);'
)
m = pat.search(s)
assert m, "widget_info block not found"
repl = (
    'row_resp.widget_info(|| {\n'
    '                        egui::WidgetInfo::labeled(\n'
    '                            egui::WidgetType::Button,\n'
    '                            true,\n'
    '                            format!("\\\\u{1f4ac} {text}{}", if s.running { " \\\\u{26a1}" } else { "" }),\n'
    '                        )\n'
    '                    });'
)
s = s[:m.start()] + repl + s[m.end():]
io.open(p, 'w', encoding='utf-8', newline='').write(s)
print('widget_info uses truncated text')
