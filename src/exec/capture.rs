//! 屏幕截图：全屏 / 指定窗口（按进程名或窗口标题定位）。
//!
//! 用 GDI BitBlt 抓屏 + PrintWindow 抓窗口（即使被遮挡），
//! 输出 PNG 到临时目录，路径返回给调用方（配合 vision 模型分析）。

#![allow(clippy::too_many_arguments)]

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    BITMAPINFO as WinBmi, BITMAPINFOHEADER, 
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    GetDC, GetDIBits, HDC, ReleaseDC, SRCCOPY,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
};

/// 截图结果。
pub struct Capture {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    /// 截图来源描述（"fullscreen" / "window: <标题>"）
    pub source: String,
}

/// 枚举窗口信息。
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub pid: u32,
    pub process_name: String,
}

/// 全屏截图。
pub fn capture_screen() -> Result<Capture, String> {
    unsafe {
        let hdc = GetDC(std::ptr::null_mut());
        if hdc.is_null() {
            return Err("无法获取屏幕 DC".into());
        }
        let w = GetSystemMetrics(0);
        let h = GetSystemMetrics(1);
        if w <= 0 || h <= 0 {
            ReleaseDC(std::ptr::null_mut(), hdc);
            return Err("无法获取屏幕尺寸".into());
        }
        let (w, h) = (w as u32, h as u32);
        let result = capture_hdc_rect(hdc, 0, 0, w, h, "fullscreen");
        ReleaseDC(std::ptr::null_mut(), hdc);
        result.map(|mut c| {
            c.width = w;
            c.height = h;
            c
        })
    }
}

/// 按窗口句柄截图（PrintWindow，即使被遮挡/后台）。
pub fn capture_window(hwnd: isize) -> Result<Capture, String> {
    unsafe {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetWindowRect(hwnd as HWND, &mut rect) == 0 {
            return Err("无法获取窗口尺寸".into());
        }
        let w = (rect.right - rect.left).max(1) as u32;
        let h = (rect.bottom - rect.top).max(1) as u32;

        let hdc_window = GetDC(hwnd as HWND);
        if hdc_window.is_null() {
            return Err("无法获取窗口 DC".into());
        }
        let hdc_mem = CreateCompatibleDC(hdc_window);
        let hbm = CreateCompatibleBitmap(hdc_window, w as i32, h as i32);
        let old = windows_sys::Win32::Graphics::Gdi::SelectObject(hdc_mem, hbm);

        // PrintWindow 可抓被遮挡窗口（PW_RENDERFULLCONTENT 支持 DWM 渲染）
        let ok = PrintWindow(hwnd as HWND, hdc_mem, 2);
        if ok == 0 {
            // 降级 BitBlt（只对前台窗口有效）
            BitBlt(hdc_mem, 0, 0, w as i32, h as i32, hdc_window, 0, 0, SRCCOPY);
        }

        let title = get_window_title(hwnd);
        let source = format!("window: {title}");

        // 从 HBITMAP 提取像素
        // 用 GetDIBits 提取
        let bi = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut pixels: Vec<u8> = vec![0; (w * h * 4) as usize];
        let mut bmi = WinBmi {
            bmiHeader: bi,
            bmiColors: [windows_sys::Win32::Graphics::Gdi::RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }; 1],
        };
        let got = GetDIBits(
            hdc_mem,
            hbm,
            0,
            h,
            pixels.as_mut_ptr() as *mut core::ffi::c_void,
            &mut bmi,
            0, // DIB_RGB_COLORS
        );
        if got == 0 {
            windows_sys::Win32::Graphics::Gdi::SelectObject(hdc_mem, old);
            DeleteObject(hbm);
            DeleteDC(hdc_mem);
            ReleaseDC(hwnd as HWND, hdc_window);
            return Err("GetDIBits 失败".into());
        }

        windows_sys::Win32::Graphics::Gdi::SelectObject(hdc_mem, old);
        DeleteObject(hbm);
        DeleteDC(hdc_mem);
        ReleaseDC(hwnd as HWND, hdc_window);

        // BGRA → RGBA
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2); // B↔R
        }

        let path = save_png(&pixels, w, h, &source)?;
        Ok(Capture { path, width: w, height: h, source })
    }
}

struct EnumCtx {
    filter: String,
    out: Vec<WindowInfo>,
}

/// 按进程名或窗口标题模糊查找窗口，返回所有匹配的可见顶层窗口。
pub fn find_windows(filter: &str) -> Vec<WindowInfo> {
    let filter_lower = filter.to_lowercase();
    let mut ctx = EnumCtx { filter: filter_lower, out: Vec::new() };

    unsafe {
        EnumWindows(
            Some(enum_proc),
            &mut ctx as *mut EnumCtx as LPARAM,
        );
    }
    ctx.out
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
    let ctx = &mut *(lparam as *mut EnumCtx);
    if IsWindowVisible(hwnd) == 0 {
        return TRUE;
    }
    // 跳过最小化的
    if IsIconic(hwnd) != 0 {
        return TRUE;
    }
    let title = get_window_title(hwnd as isize);
    if title.is_empty() {
        return TRUE;
    }

    // 获取进程 ID 和进程名
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    let process_name = get_process_name(pid);

    let title_lower = title.to_lowercase();
    let proc_lower = process_name.to_lowercase();
    if title_lower.contains(&ctx.filter) || proc_lower.contains(&ctx.filter) {
        ctx.out.push(WindowInfo {
            hwnd: hwnd as isize,
            title,
            pid,
            process_name,
        });
    }
    TRUE
}

fn get_window_title(hwnd: isize) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd as HWND);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let n = GetWindowTextW(hwnd as HWND, buf.as_mut_ptr(), len + 1);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

fn get_process_name(pid: u32) -> String {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    let handle = unsafe {
        OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid)
    };
    if handle.is_null() {
        return String::new();
    }
    // 用 QueryFullProcessImageNameW
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        windows_sys::Win32::System::Threading::QueryFullProcessImageNameW(
            handle,
            0,
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }
    if ok == 0 {
        return String::new();
    }
    let full = String::from_utf16_lossy(&buf[..len as usize]);
    full.rsplit('\\').next().unwrap_or("").to_string()
}

/// 从 DC 抓指定矩形区域。
unsafe fn capture_hdc_rect(
    hdc: HDC,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    source: &str,
) -> Result<Capture, String> {
    let hdc_mem = CreateCompatibleDC(hdc);
    let hbm = CreateCompatibleBitmap(hdc, w as i32, h as i32);
    let old = windows_sys::Win32::Graphics::Gdi::SelectObject(hdc_mem, hbm);

    BitBlt(hdc_mem, 0, 0, w as i32, h as i32, hdc, x, y, SRCCOPY);

    let bi = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: -(h as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut pixels: Vec<u8> = vec![0; (w * h * 4) as usize];
    let mut bmi = WinBmi {
        bmiHeader: bi,
        bmiColors: [windows_sys::Win32::Graphics::Gdi::RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }; 1],
    };
    let got = GetDIBits(
        hdc_mem,
        hbm,
        0,
        h,
        pixels.as_mut_ptr() as *mut core::ffi::c_void,
        &mut bmi,
        0,
    );
    windows_sys::Win32::Graphics::Gdi::SelectObject(hdc_mem, old);
    DeleteObject(hbm);
    DeleteDC(hdc_mem);

    if got == 0 {
        return Err("GetDIBits 失败".into());
    }
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let path = save_png(&pixels, w, h, source)?;
    Ok(Capture { path, width: w, height: h, source: source.to_string() })
}

/// 保存 PNG 到临时目录。
fn save_png(
    rgba: &[u8],
    w: u32,
    h: u32,
    source: &str,
) -> Result<PathBuf, String> {
    let img = image::RgbaImage::from_raw(w, h, rgba.to_vec())
        .ok_or("像素数据尺寸不匹配")?;
    let dir = std::env::temp_dir().join("dsh-screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let safe: String = source
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(30)
        .collect();
    let path = dir.join(format!("{safe}_{ts}.png"));
    img.save(&path).map_err(|e| format!("保存 PNG 失败: {e}"))?;
    Ok(path)
}

