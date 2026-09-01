//! 鼠标 / 键盘输入注入（SendInput）。
//!
//! 坐标体系与 take_screenshot 一致：**屏幕物理像素**（进程 DPI aware，
//! GDI 截图与 SetCursorPos 同为物理像素——截图上看到什么坐标就点什么）。
//! 供 agent 的 mouse_click / key_type 等工具调用（配合 take_screenshot
//! 先定位目标再操作）。

#![allow(clippy::too_many_arguments)]

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE,
    VK_F1, VK_F10, VK_F11, VK_F12, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8,
    VK_F9, VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP, VIRTUAL_KEY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;

const WHEEL_DELTA_NOTCH: i32 = 120;

/// 鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "left" | "" => Some(Self::Left),
            "right" => Some(Self::Right),
            "middle" | "mid" => Some(Self::Middle),
            _ => None,
        }
    }
}

fn flags_for(button: MouseButton, down: bool) -> MOUSE_EVENT_FLAGS {
    match (button, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> bool {
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        ) == inputs.len() as u32
    }
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// 移动鼠标到屏幕物理像素坐标。
pub fn mouse_move(x: i32, y: i32) -> Result<(), String> {
    if unsafe { SetCursorPos(x, y) } == 0 {
        return Err(format!("SetCursorPos({x},{y}) 失败"));
    }
    Ok(())
}

/// 点击鼠标（可选双击）。先移动到 (x, y) 再按下/抬起。
pub fn mouse_click(
    x: i32,
    y: i32,
    button: MouseButton,
    double: bool,
) -> Result<(), String> {
    mouse_move(x, y)?;
    sleep_ms(30);
    let down = flags_for(button, true);
    let up = flags_for(button, false);
    let mut inputs = vec![mouse_input(down, 0), mouse_input(up, 0)];
    if double {
        sleep_ms(40);
        inputs.extend([mouse_input(down, 0), mouse_input(up, 0)]);
    }
    if !send(&inputs) {
        return Err("SendInput(点击) 失败".into());
    }
    Ok(())
}

/// 按住拖拽：移动到起点 → 按下 → 移动到终点（中间插值几步，拖拽更稳）→ 抬起。
pub fn mouse_drag(
    from: (i32, i32),
    to: (i32, i32),
    button: MouseButton,
) -> Result<(), String> {
    mouse_move(from.0, from.1)?;
    sleep_ms(50);
    if !send(&[mouse_input(flags_for(button, true), 0)]) {
        return Err("SendInput(按下) 失败".into());
    }
    // 插值移动（8 步）——很多程序依赖拖拽过程中的移动事件
    for i in 1..=8 {
        let t = i as f32 / 8.0;
        let mx = from.0 as f32 + (to.0 as f32 - from.0 as f32) * t;
        let my = from.1 as f32 + (to.1 as f32 - from.1 as f32) * t;
        mouse_move(mx.round() as i32, my.round() as i32)?;
        sleep_ms(15);
    }
    sleep_ms(40);
    if !send(&[mouse_input(flags_for(button, false), 0)]) {
        return Err("SendInput(抬起) 失败".into());
    }
    Ok(())
}

/// 滚轮（amount 为格数；direction "up"/"down"；可先移动到 x,y）。
pub fn mouse_scroll(
    x: Option<i32>,
    y: Option<i32>,
    amount: i32,
    up: bool,
) -> Result<(), String> {
    if let (Some(x), Some(y)) = (x, y) {
        mouse_move(x, y)?;
        sleep_ms(20);
    }
    let notches = amount.clamp(1, 30) * WHEEL_DELTA_NOTCH * if up { 1 } else { -1 };
    if !send(&[mouse_input(MOUSEEVENTF_WHEEL, notches)]) {
        return Err("SendInput(滚轮) 失败".into());
    }
    Ok(())
}

/// 用 Unicode 事件逐字符输入文本（支持任意语言；\n 转为回车键）。
pub fn key_type_text(text: &str) -> Result<(), String> {
    let mut inputs: Vec<INPUT> = Vec::new();
    for ch in text.chars() {
        if ch == '\n' {
            inputs.push(key_input(VK_RETURN, 0, 0));
            inputs.push(key_input(VK_RETURN, 0, KEYEVENTF_KEYUP));
            continue;
        }
        let code = ch as u32;
        if code > 0xFFFF {
            return Err(format!("无法输入的字符（超出 BMP）: {ch:?}"));
        }
        inputs.push(key_input(0, code as u16, KEYEVENTF_UNICODE));
        inputs.push(key_input(0, code as u16, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    if inputs.is_empty() {
        return Ok(());
    }
    // 分批发送（一次超长输入可能被系统丢弃）
    for chunk in inputs.chunks(64) {
        if !send(chunk) {
            return Err("SendInput(键入) 失败".into());
        }
        sleep_ms(5);
    }
    Ok(())
}

/// 解析组合键字符串为虚拟键序列（修饰键在前、主键最后）。
/// 支持："ctrl+s"、"ctrl+shift+t"、"alt+f4"、"win+r"、"enter"、"esc"、
/// "tab"、"backspace"、"delete"、"home"/"end"/"pgup"/"pgdn"、方向键、
/// f1-f12、"space"、单个字母/数字。
pub fn parse_combo(combo: &str) -> Result<Vec<VIRTUAL_KEY>, String> {
    let mut mods: Vec<VIRTUAL_KEY> = Vec::new();
    let mut main: Option<VIRTUAL_KEY> = None;
    for part in combo.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let key = part.to_ascii_lowercase();
        let vk = match key.as_str() {
            "ctrl" | "control" => Some(VK_CONTROL),
            "shift" => Some(VK_SHIFT),
            "alt" => Some(VK_MENU),
            "win" | "meta" | "super" => Some(VK_LWIN),
            _ => None,
        };
        if let Some(vk) = vk {
            if !mods.contains(&vk) {
                mods.push(vk);
            }
            continue;
        }
        if main.is_some() {
            return Err(format!("组合键只能有一个主键: {combo}"));
        }
        main = Some(parse_key(&key).ok_or_else(|| format!("未知按键: {part}"))?);
    }
    let main = main.ok_or_else(|| format!("组合键缺少主键: {combo}"))?;
    mods.push(main);
    Ok(mods)
}

/// 单个键名 → 虚拟键码。
pub fn parse_key(name: &str) -> Option<VIRTUAL_KEY> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => VK_RETURN,
        "esc" | "escape" => VK_ESCAPE,
        "tab" => VK_TAB,
        "backspace" | "bs" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "home" => VK_HOME,
        "end" => VK_END,
        "pgup" | "pageup" => VK_PRIOR,
        "pgdn" | "pagedown" => VK_NEXT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "space" => VK_SPACE,
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        s => {
            let mut chars = s.chars();
            let (c1, c2) = (chars.next(), chars.next());
            match (c1, c2) {
                // 单个字母/数字 → VK 码（'A'..='Z' = 0x41..，'0'..='9' = 0x30..）
                (Some(c), None) if c.is_ascii_alphabetic() => {
                    c.to_ascii_uppercase() as u16
                }
                (Some(c), None) if c.is_ascii_digit() => c as u16,
                _ => return None,
            }
        }
    })
}

/// 按下组合键（修饰键按下 → 主键按下/抬起 → 修饰键抬起）。
pub fn key_press_combo(combo: &str) -> Result<(), String> {
    let vks = parse_combo(combo)?;
    let mut inputs: Vec<INPUT> = Vec::new();
    for &vk in &vks {
        inputs.push(key_input(vk, 0, 0));
    }
    for &vk in vks.iter().rev() {
        inputs.push(key_input(vk, 0, KEYEVENTF_KEYUP));
    }
    if !send(&inputs) {
        return Err("SendInput(组合键) 失败".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combo_parsing() {
        let vks = parse_combo("ctrl+s").unwrap();
        assert_eq!(vks, vec![VK_CONTROL, 'S' as u16]);
        let vks = parse_combo("Ctrl + Shift + T").unwrap();
        assert_eq!(vks, vec![VK_CONTROL, VK_SHIFT, 'T' as u16]);
        let vks = parse_combo("alt+f4").unwrap();
        assert_eq!(vks, vec![VK_MENU, VK_F4]);
        let vks = parse_combo("win+r").unwrap();
        assert_eq!(vks, vec![VK_LWIN, 'R' as u16]);
        let vks = parse_combo("enter").unwrap();
        assert_eq!(vks, vec![VK_RETURN]);
        let vks = parse_combo("pgdn").unwrap();
        assert_eq!(vks, vec![VK_NEXT]);
        // 重复修饰键去重
        let vks = parse_combo("ctrl+ctrl+a").unwrap();
        assert_eq!(vks, vec![VK_CONTROL, 'A' as u16]);
        // 错误输入
        assert!(parse_combo("ctrl+").is_err(), "缺主键");
        assert!(parse_combo("hello").is_err(), "未知按键");
        assert!(parse_combo("a+b").is_err(), "两个主键");
    }

    #[test]
    fn key_names() {
        assert_eq!(parse_key("ESC"), Some(VK_ESCAPE));
        assert_eq!(parse_key("backspace"), Some(VK_BACK));
        assert_eq!(parse_key("5"), Some('5' as u16));
        assert_eq!(parse_key("f12"), Some(VK_F12));
        assert_eq!(parse_key("nl"), None);
    }

    #[test]
    fn button_parse() {
        assert_eq!(MouseButton::parse("left"), Some(MouseButton::Left));
        assert_eq!(MouseButton::parse("RIGHT"), Some(MouseButton::Right));
        assert_eq!(MouseButton::parse("mid"), Some(MouseButton::Middle));
        assert_eq!(MouseButton::parse("x"), None);
    }
}
