//! 纯 Rust Win32 restricted-token 沙箱（完整实现）。
//!
//! 完全用 `windows-sys` 直接写 Win32（无任何 Node 依赖），实现：
//! - SID：Everyone、logon session SID、capability SID（路径派生）
//! - Token：CreateRestrictedToken（限制 SID = logon + Everyone + 可选 workspace）
//! - ACL：GetNamedSecurityInfoW + SetEntriesInAclW + SetNamedSecurityInfoW 授权目录写
//! - Spawn：CreateProcessAsUserW 用受限 token + 匿名管道 stdio + 超时 kill
//!
//! 全程 fail-closed：任何 Win32 失败即报错，绝不用未受限令牌 spawn。

#![allow(non_snake_case, unused)]

use std::ffi::c_void;
use std::path::Path;
use std::ptr;

use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, SetHandleInformation, FALSE, HANDLE, HLOCAL, TRUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, CreateWellKnownSid, GetTokenInformation, TokenGroups, WinNullSid,
    WinWorldSid, ACL, DACL_SECURITY_INFORMATION as DACL_SEC_INFO, SID, SID_AND_ATTRIBUTES,
    TOKEN_ALL_ACCESS, TOKEN_GROUPS,
};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::SE_GROUP_LOGON_ID;
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread,
    TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

type WResult<T> = Result<T, String>;

fn win_err(api: &str) -> String {
    format!("{api} failed (Win32 {})", unsafe { GetLastError() })
}

/// 扁平 SID（SID 结构 + 变长尾部）。
#[derive(Clone)]
pub struct RawSid {
    bytes: Vec<u8>,
}
impl RawSid {
    pub fn as_ptr(&self) -> *const c_void {
        self.bytes.as_ptr() as *const c_void
    }
    pub fn as_sid(&self) -> *const SID {
        self.bytes.as_ptr() as *const SID
    }
    pub fn to_string(&self) -> String {
        // 手工解析 SID 字节：rev@0,count@1,authority(6)@2,sub(LE u32)@8..
        let b = std::panic::catch_unwind(|| {
            if self.bytes.len() < 8 {
                return "S-1-0".to_string();
            }
            let count = self.bytes[1] as usize;
            let mut a: u64 = 0;
            for &x in &self.bytes[2..8] {
                a = (a << 8) | x as u64;
            }
            let mut s = format!("S-1-{a}");
            let mut off = 8usize;
            for _ in 0..count.min((self.bytes.len() - 8) / 4) {
                if off + 4 > self.bytes.len() {
                    break;
                }
                let v = u32::from_le_bytes([
                    self.bytes[off],
                    self.bytes[off + 1],
                    self.bytes[off + 2],
                    self.bytes[off + 3],
                ]);
                s.push_str(&format!("-{v}"));
                off += 4;
            }
            s
        });
        b.unwrap_or_else(|_| "S-1-0".to_string())
    }
    fn from_sid_ptr(p: *const c_void) -> Self {
        let sid = unsafe { &*(p as *const SID) };
        let n = 8 + sid.SubAuthorityCount as usize * 4;
        let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, n).to_vec() };
        RawSid { bytes }
    }
}

fn open_current_token() -> WResult<HANDLE> {
    let mut t: HANDLE = ptr::null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut t) };
    if ok == 0 {
        return Err(win_err("OpenProcessToken"));
    }
    Ok(t)
}

/// Everyone SID (S-1-1-0)。
pub fn everyone_sid() -> WResult<RawSid> {
    let mut buf = [0u8; 68];
    let mut len: u32 = 68;
    let ok = unsafe {
        CreateWellKnownSid(
            WinWorldSid,
            ptr::null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(win_err("CreateWellKnownSid"));
    }
    Ok(RawSid {
        bytes: buf[..len as usize].to_vec(),
    })
}

/// logon session SID（SE_GROUP_LOGON_ID），找不到回退 Everyone。
pub fn logon_sid() -> WResult<RawSid> {
    let token = open_current_token()?;
    let mut size: u32 = 0;
    unsafe {
        GetTokenInformation(token, TokenGroups, ptr::null_mut(), 0, &mut size);
    }
    let mut buf = vec![0u8; size as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            buf.as_mut_ptr() as *mut c_void,
            size,
            &mut size,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(win_err("GetTokenInformation(TokenGroups)"));
    }
    let groups = unsafe { &*(buf.as_ptr() as *const TOKEN_GROUPS) };
    let gs =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    for g in gs {
        if (g.Attributes & SE_GROUP_LOGON_ID as u32) != 0 {
            return Ok(RawSid::from_sid_ptr(g.Sid));
        }
    }
    everyone_sid()
}

/// 由路径派生 capability SID（S-1-4-<a>-<b>，与官方 workspace-sid 哈希算法一致）。
/// sha256(路径) 前 8 字节 → 两个 30-bit 子权威（+1 保非零）。
pub fn capability_sid_from_path(path: &Path) -> WResult<RawSid> {
    use sha2::{Digest, Sha256};
    let p = path.to_string_lossy().into_owned();
    let mut h = Sha256::new();
    h.update(p.as_bytes());
    let digest = h.finalize();
    let a = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let b = u32::from_le_bytes([digest[4], digest[5], digest[6], digest[7]]);
    let a = (a % ((1 << 30) - 1)) + 1;
    let b = (b % ((1 << 30) - 1)) + 1;
    string_to_sid(&format!("S-1-4-{a}-{b}"))
}

fn string_to_sid(s: &str) -> WResult<RawSid> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() < 4 || parts[0] != "S" {
        return Err(format!("invalid SID: {s}"));
    }
    let authority: u64 = parts[2].parse().map_err(|_| "bad authority")?;
    let mut buf = Vec::with_capacity(8 + (parts.len() - 3) * 4);
    buf.push(1u8);
    buf.push((parts.len() - 3) as u8);
    let ab = authority.to_be_bytes();
    buf.extend_from_slice(&ab[2..]);
    for p in &parts[3..] {
        let v: u32 = p.parse().map_err(|_| "bad subauthority")?;
        buf.extend_from_slice(&v.to_le_bytes());
    }
    Ok(RawSid { bytes: buf })
}

/// 构建受限 token：限制 SID = logon + Everyone + extra(workspace)。
pub fn build_restricted_token(extra: &[RawSid]) -> WResult<HANDLE> {
    let token = open_current_token()?;
    let logon = logon_sid()?;
    let everyone = everyone_sid()?;
    let mut list: Vec<RawSid> = vec![logon, everyone];
    list.extend_from_slice(extra);
    let mut sids: Vec<SID_AND_ATTRIBUTES> = list
        .iter()
        .map(|s| SID_AND_ATTRIBUTES {
            Sid: s.as_ptr() as *mut c_void,
            Attributes: 0,
        })
        .collect();
    let mut restricted: HANDLE = ptr::null_mut();
    let ok = unsafe {
        CreateRestrictedToken(
            token,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            sids.len() as u32,
            sids.as_ptr(),
            &mut restricted,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(win_err("CreateRestrictedToken"));
    }
    Ok(restricted)
}

const FILE_GENERIC_WRITE_DELETE: u32 = 0x12019F | 0x10000;
const CONTAINER_AND_OBJECT_INHERIT: u32 = 0x3;

/// 给目录授予 capability-SID 的写 ACE（写 DACL；幂等，保留既有 ACE）。
pub fn grant_dir_write(dir: &Path, sid: &RawSid) -> WResult<()> {
    let dirw = wide(dir);
    let mut ppsd: *mut c_void = ptr::null_mut();
    let ok = unsafe {
        GetNamedSecurityInfoW(
            dirw.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SEC_INFO,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut ppsd,
        )
    };
    if ok != 0 {
        return Err(win_err("GetNamedSecurityInfoW"));
    }
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_GENERIC_WRITE_DELETE,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: CONTAINER_AND_OBJECT_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: sid.as_ptr() as *mut u16,
        },
    };
    let mut new_acl: *mut ACL = ptr::null_mut();
    let ok = unsafe { SetEntriesInAclW(1, &ea, ppsd as *const ACL, &mut new_acl) };
    if ok == 0 {
        return Err(win_err("SetEntriesInAclW"));
    }
    let ok = unsafe {
        SetNamedSecurityInfoW(
            dirw.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SEC_INFO,
            ptr::null_mut(),
            ptr::null_mut(),
            new_acl,
            ptr::null_mut(),
        )
    };
    // 释放 SetEntriesInAclW / GetNamedSecurityInfoW 分配的内存
    if !new_acl.is_null() {
        unsafe { LocalFree(new_acl as HLOCAL) };
    }
    if !ppsd.is_null() {
        unsafe { LocalFree(ppsd as HLOCAL) };
    }
    if ok != 0 {
        return Err(win_err("SetNamedSecurityInfoW"));
    }
    Ok(())
}

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 受限方式执行命令：用 restricted token 经 CreateProcessAsUserW 启动，
/// 匿名管道捕获 stdout/stderr，超时 kill。返回 (exit_code, stdout_bytes, stderr_bytes)。
/// fail-closed：token 构建或 CreateProcessAsUserW 失败即返回 Err，绝不直通。
pub fn spawn_restricted(
    restricted_token: HANDLE,
    command: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> WResult<(i32, Vec<u8>, Vec<u8>)> {
    let mut command_line = command.to_string();
    for a in args {
        command_line.push(' ');
        command_line.push_str(a);
    }
    let mut command_line_w: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let cwd_w = wide(cwd);

    // 三组管道：(读端, 写端)
    let (stdin_r, stdin_w) = make_pipe()?;
    let (out_r, out_w) = make_pipe()?;
    let (err_r, err_w) = make_pipe()?;
    // 子进程侧才需要继承的端：stdin 读、stdout 写、stderr 写
    set_inherit(stdin_r, "stdin_r")?;
    set_inherit(out_w, "out_w")?;
    set_inherit(err_w, "err_w")?;
    // 父进程不写 stdin：关掉写端
    unsafe { CloseHandle(stdin_w) };

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = stdin_r;
    si.hStdOutput = out_w;
    si.hStdError = err_w;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessAsUserW(
            restricted_token,
            ptr::null(),
            command_line_w.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            TRUE,
            CREATE_SUSPENDED,
            ptr::null(),
            cwd_w.as_ptr(),
            &si,
            &mut pi,
        )
    };
    // 创建后：父进程关闭子进程复制的写端 + stdin 读端，避免读取线程等不到 EOF
    unsafe { CloseHandle(out_w) };
    unsafe { CloseHandle(err_w) };
    unsafe { CloseHandle(stdin_r) };
    if ok == 0 {
        return Err(win_err("CreateProcessAsUserW"));
    }

    let out_r_keep = out_r;
    let err_r_keep = err_r;
    let start = Instant::now();
    unsafe { ResumeThread(pi.hThread) };

    // 等待退出或超时
    loop {
        let r = unsafe { WaitForSingleObject(pi.hProcess, 0) };
        if r == WAIT_OBJECT_0 {
            break;
        }
        if start.elapsed() > timeout {
            unsafe { TerminateProcess(pi.hProcess, 1) };
            unsafe { WaitForSingleObject(pi.hProcess, u32::MAX) };
            let stdout = drain_pipe(out_r_keep);
            let stderr = drain_pipe(err_r_keep);
            unsafe {
                CloseHandle(pi.hProcess);
                CloseHandle(pi.hThread)
            };
            unsafe {
                CloseHandle(out_r_keep);
                CloseHandle(err_r_keep)
            };
            return Ok((1, stdout, stderr));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let stdout = drain_pipe(out_r_keep);
    let stderr = drain_pipe(err_r_keep);
    let mut code: u32 = 0;
    unsafe { GetExitCodeProcess(pi.hProcess, &mut code) };
    unsafe {
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread)
    };
    unsafe {
        CloseHandle(out_r_keep);
        CloseHandle(err_r_keep)
    };
    Ok((code as i32, stdout, stderr))
}

// —— 管道与读取辅助 ——

struct ReadHandle {
    h: HANDLE,
}
unsafe impl Send for ReadHandle {}

fn make_pipe() -> WResult<(HANDLE, HANDLE)> {
    let mut r: HANDLE = ptr::null_mut();
    let mut w: HANDLE = ptr::null_mut();
    let ok = unsafe { CreatePipe(&mut r, &mut w, ptr::null(), 0) };
    if ok == 0 {
        return Err(win_err("CreatePipe"));
    }
    Ok((r, w))
}

fn set_inherit(h: HANDLE, label: &str) -> WResult<()> {
    // HANDLE_FLAG_INHERIT = 0x1
    if unsafe { SetHandleInformation(h, 0x1, 0x1) } == 0 {
        return Err(format!(
            "SetHandleInformation({label}) failed (Win32 {})",
            unsafe { GetLastError() }
        ));
    }
    Ok(())
}

fn drain_pipe(h: HANDLE) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let mut read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break;
        }
        out.extend_from_slice(&buf[..read as usize]);
        if out.len() > (1 << 20) {
            // 上限 1MB
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use windows_sys::Win32::Foundation::CloseHandle;

    /// SID 字符串渲染往返。
    #[test]
    fn sid_string_roundtrip() {
        let e = everyone_sid().unwrap();
        assert_eq!(e.to_string(), "S-1-1-0");
        let cap = capability_sid_from_path(Path::new(r"C:\workspace")).unwrap();
        assert!(
            cap.to_string().starts_with("S-1-4-"),
            "cap sid: {}",
            cap.to_string()
        );
    }

    /// 受限 token 构建成功（本机验证）。
    #[test]
    fn restricted_token_builds() {
        let everyone = everyone_sid().unwrap();
        let logon = logon_sid().unwrap();
        let cap = capability_sid_from_path(Path::new(r"C:\workspace")).unwrap();
        eprintln!(
            "everyone={} logon={} cap={}",
            everyone.to_string(),
            logon.to_string(),
            cap.to_string()
        );
        // 方案A：仅限制 everyone（隔离 capability SID 是否导致问题）
        let t_every = build_restricted_token(&[]).expect("仅 everyone + logon 应能构建");
        unsafe { CloseHandle(t_every) };
        // 方案B：带 capability SID
        let token = build_restricted_token(&[cap]).expect("带 capability SID 应能构建");
        assert!(!token.is_null());
        unsafe { CloseHandle(token) };
        assert!(!token.is_null());
        unsafe { CloseHandle(token) };
    }

    /// 真实受限 spawn + 写隔离（依赖本机权限；默认 #[ignore]）。
    /// ws 内写成功、ws 外写被拒。
    #[test]
    #[ignore]
    fn e2e_workspace_write_isolation() {
        let dir = tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("inside.txt"), "x").unwrap();
        let cap = capability_sid_from_path(&ws).unwrap();
        // 授权 ws 可写
        grant_dir_write(&ws, &cap).expect("grant ws 写应成功");
        let token = build_restricted_token(&[cap]).expect("受限 token");
        let timeout = Duration::from_secs(30);

        // 1) 写 ws 内 → 应成功
        let cwd = std::env::temp_dir();
        let target = ws.join("inside2.txt");
        let script = format!("echo inside > \"{}\"", target.display());
        let (code, out, err) =
            spawn_restricted(token, "cmd", &["/C", &script], &cwd, timeout).expect("spawn 应成功");
        let out_s = String::from_utf8_lossy(&out);
        let err_s = String::from_utf8_lossy(&err);
        assert!(
            code == 0 || out_s.contains("inside") || err_s.trim().is_empty(),
            "写 ws 内应成功, code={code} out={out_s} err={err_s}"
        );
        // 2) 写 ws 外 → 应被拒（受限 token）
        let outside = dir.path().join("escape.txt");
        let script2 = format!("echo hi > \"{}\"", outside.display());
        let (_code2, _o2, _e2) =
            spawn_restricted(token, "cmd", &["/C", &script2], &cwd, timeout).expect("spawn 应成功");
        assert!(!outside.exists(), "ws 外文件绝不能被创建");
        unsafe { CloseHandle(token) };
    }
}
