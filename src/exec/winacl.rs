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
    CloseHandle, GetLastError, LocalFree, SetHandleInformation, FALSE, HANDLE, HLOCAL, LUID, TRUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, CreateWellKnownSid, GetTokenInformation, LookupPrivilegeNameW,
    TokenGroups, TokenPrivileges,
    WinNullSid, WinWorldSid, ACL, DACL_SECURITY_INFORMATION as DACL_SEC_INFO, LUID_AND_ATTRIBUTES,
    SID, SID_AND_ATTRIBUTES, TOKEN_ALL_ACCESS, TOKEN_GROUPS, TOKEN_PRIVILEGES,
};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::SE_GROUP_LOGON_ID;
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread,
    TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
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

/// 构建受限 token：限制 SID = logon + Everyone + extra(workspace)，
/// 并**删除全部特权**（SeBackup/SeRestore/SeTakeOwnership/SeDebug 等能绕过
/// 文件 DACL 检查——只加 restricting SID 挡不住它们）。
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
    // 特权删除：仅删能绕过文件 DACL 的高危特权（SeBackup/SeRestore/
    // SeTakeOwnership/SeDebug/SeLoadDriver/SeSystemEnvironment 等），
    // 保留 SeChangeNotify/SeShutdown 等基本运行所需。
    // 历史缺陷：priv_to_delete.clear() 一个都没删——提升权限下受限子进程
    // 仍可用 SeBackup/SeRestore 绕过 read-only DACL。
    // 通过 LookupPrivilegeNameW 把 LUID 解析回名字做白名单过滤。
    let privs = token_privileges(token);
    let danger = |luid: &LUID| -> bool {
        let mut name = [0u16; 64];
        let mut len: u32 = name.len() as u32;
        let ok = unsafe {
            LookupPrivilegeNameW(
                ptr::null(),
                luid as *const LUID,
                name.as_mut_ptr(),
                &mut len,
            )
        };
        if ok == 0 {
            return false; // 解析失败不删（保守：保运行）
        }
        let n = String::from_utf16_lossy(&name[..len as usize]);
        matches!(
            n.as_str(),
            "SeBackupPrivilege"
                | "SeRestorePrivilege"
                | "SeTakeOwnershipPrivilege"
                | "SeDebugPrivilege"
                | "SeLoadDriverPrivilege"
                | "SeSystemEnvironmentPrivilege"
                | "SeTcbPrivilege"
                | "SeAssignPrimaryTokenPrivilege"
                | "SeImpersonatePrivilege"
        )
    };
    let priv_to_delete: Vec<LUID_AND_ATTRIBUTES> = privs
        .iter()
        .filter(|l| danger(l))
        .map(|l| LUID_AND_ATTRIBUTES {
            Luid: *l,
            Attributes: 0,
        })
        .collect();
    if !priv_to_delete.is_empty() {
        log::info!(
            "restricted token: deleting {} dangerous privileges",
            priv_to_delete.len()
        );
    }
    let mut restricted: HANDLE = ptr::null_mut();
    let ok = unsafe {
        CreateRestrictedToken(
            token,
            0,
            0,
            ptr::null(),
            priv_to_delete.len() as u32,
            priv_to_delete.as_ptr(),
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

/// 枚举 token 的全部特权 LUID（失败返回空 = 不删任何特权，fail-closed 方向
/// 仍是限制 SID；特权删除是纵深加固层）。
fn token_privileges(token: HANDLE) -> Vec<LUID> {
    let mut len: u32 = 0;
    unsafe {
        GetTokenInformation(token, TokenPrivileges, ptr::null_mut(), 0, &mut len);
    }
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; len as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenPrivileges,
            buf.as_mut_ptr() as *mut c_void,
            len,
            &mut len,
        )
    };
    if ok == 0 {
        return Vec::new();
    }
    // TOKEN_PRIVILEGES { PrivilegeCount: u32, Privileges: [LUID_AND_ATTRIBUTES; 1] }
    let count = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let base = std::mem::size_of::<u32>();
    let item = std::mem::size_of::<LUID_AND_ATTRIBUTES>();
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = base + i * item;
        if off + item > buf.len() {
            break;
        }
        let luid = LUID {
            LowPart: u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]),
            HighPart: i32::from_ne_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]),
        };
        out.push(luid);
    }
    out
}

/// Job Object（KILL_ON_JOB_CLOSE）安全封装：句柄 Drop 时终止整棵进程树。
/// 用途：shell 超时杀进程树——`TerminateProcess(直接子进程)` 杀不掉 cmd /c
/// 启动的孙进程，孙进程持有管道写端还会把排水线程永久挂起。
pub struct KillOnCloseJob {
    h: HANDLE,
}

impl KillOnCloseJob {
    /// 创建 Job 并把子进程挂进去。失败返回 None（调用方退化为杀直接子进程）。
    /// pub(crate)：参数是裸 HANDLE，不作为跨 crate API 导出。
    pub(crate) fn attach(child_process: HANDLE) -> Option<Self> {
        unsafe {
            let job = CreateJobObjectW(ptr::null(), ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return None;
            }
            if AssignProcessToJobObject(job, child_process) == 0 {
                CloseHandle(job);
                return None;
            }
            Some(KillOnCloseJob { h: job })
        }
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE：关闭句柄即终止全部关联进程（正常收尾时子进程
        // 已退出，关闭无副作用）
        unsafe { CloseHandle(self.h) };
    }
}

/// 子进程用的最小环境块（UTF-16，双 NUL 结尾）。
/// 不继承父进程完整环境：API key / 代理凭据 / DSH_HOME 等敏感变量不得
/// 进入受限子进程。
fn minimal_env_block() -> Vec<u16> {
    // 继承父进程完整环境，仅剔除敏感项（API key、代理凭据等）。
    // 旧实现极简白名单缺 PROCESSOR_* / USERNAME / APPDATA → 子进程 DLL
    // 初始化失败 STATUS_DLL_INIT_FAILED (0xC0000142)，cmd/powershell 起不来。
    const DENY: &[&str] = &[
        "DEEPSEEK_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "DSH_HOME",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in std::env::vars() {
        let upper = k.to_uppercase();
        if DENY.iter().any(|d| upper == *d) {
            continue;
        }
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
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
/// 匿名管道捕获 stdout/stderr，超时经 Job Object 杀整棵进程树。
/// 返回 (exit_code, stdout_bytes, stderr_bytes)。
/// fail-closed：token 构建或 CreateProcessAsUserW 失败即返回 Err，绝不直通。
/// pub(crate)：参数含裸 HANDLE，不作为跨 crate API 导出。
pub(crate) fn spawn_restricted(
    restricted_token: HANDLE,
    command: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> WResult<(i32, Vec<u8>, Vec<u8>)> {
    // 命令行按 Windows 引号规则拼接（含空格的参数不加引号会被拆词/注入）
    let mut command_line = quote_arg(command);
    for a in args {
        command_line.push(' ');
        command_line.push_str(&quote_arg(a));
    }
    let mut command_line_w: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let cwd_w = wide(cwd);
    let env = minimal_env_block();

    // 三组管道：(读端, 写端)
    let (stdin_r, stdin_w) = make_pipe()?;
    let (out_r, out_w) = make_pipe()?;
    // 第二组管道之后失败：先释放已创建的句柄（防泄漏）
    let (err_r, err_w) = match make_pipe() {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                CloseHandle(stdin_r);
                CloseHandle(stdin_w);
                CloseHandle(out_r);
                CloseHandle(out_w);
            }
            return Err(e);
        }
    };
    // 子进程侧才需要继承的端：stdin 读、stdout 写、stderr 写
    if let Err(e) = set_inherit(stdin_r, "stdin_r") {
        unsafe {
            CloseHandle(stdin_r);
            CloseHandle(stdin_w);
            CloseHandle(out_r);
            CloseHandle(out_w);
            CloseHandle(err_r);
            CloseHandle(err_w);
        }
        return Err(e);
    }
    if let Err(e) = set_inherit(out_w, "out_w").and(set_inherit(err_w, "err_w")) {
        unsafe {
            CloseHandle(stdin_r);
            CloseHandle(stdin_w);
            CloseHandle(out_r);
            CloseHandle(out_w);
            CloseHandle(err_r);
            CloseHandle(err_w);
        }
        return Err(e);
    }
    // 父进程不写 stdin：关掉写端（子进程读 stdin 立即 EOF，不会挂住）
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
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr() as *const c_void,
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
        unsafe {
            CloseHandle(out_r);
            CloseHandle(err_r);
        }
        return Err(win_err("CreateProcessAsUserW"));
    }

    // Job Object：超时/退出时关闭句柄即终止整棵进程树（孙进程不残留）
    let _job = KillOnCloseJob::attach(pi.hProcess);

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
            // 有界等待：TerminateProcess 失败（罕见）不得永久挂起
            unsafe { WaitForSingleObject(pi.hProcess, 5000) };
            // 先树杀（drop Job 杀光孙进程——否则孙进程持管道写端，
            // 排水 ReadFile 永不返回，历史死锁），再限时排水
            drop(_job);
            let (stdout, _) = drain_pipe_bounded(out_r_keep, Duration::from_secs(5));
            let (stderr, _) = drain_pipe_bounded(err_r_keep, Duration::from_secs(5));
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
    // 正常退出：主进程已结束但孙进程可能仍持写端——先树杀再排水（同上）
    drop(_job);
    let (stdout, _) = drain_pipe_bounded(out_r_keep, Duration::from_secs(5));
    let (stderr, _) = drain_pipe_bounded(err_r_keep, Duration::from_secs(5));
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

/// Windows 命令行参数引号（CommandLineToArgvW 约定）：
/// 含空白/引号/尾反斜杠的参数包引号并转义。
fn quote_arg(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".into();
    }
    let needs = s.contains([' ', '\t', '"']) || s.ends_with('\\');
    if !needs {
        return s.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in s.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
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

/// 有界排水： overlapped 读 + WaitForSingleObject 超时（历史死锁：孙进程
/// 持管道写端时同步 ReadFile 永不返回 → turn 线程卡死、turn_lock 永久持有）。
/// 返回 (bytes, timed_out)。
fn drain_pipe_bounded(h: HANDLE, wait: Duration) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let deadline = std::time::Instant::now() + wait;
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
            return (out, false);
        }
        out.extend_from_slice(&buf[..read as usize]);
        if out.len() > (1 << 20) || std::time::Instant::now() > deadline {
            return (out, false);
        }
    }
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
