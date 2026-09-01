//! 复现测试：del /f /q 里的 /f（force 标志）不应被当作文件路径触发审批。
//! 历史 bug：/f 被 extract_redirect_targets 采集 → resolve("/f") → 盘根
//! join "" → 审批弹「目标 F:/f 不在工作区内」。
//!
//! 路径全部基于临时目录构造（历史版本硬编码 F:\AI，机器迁移后
//! 祖先回溯失败导致误判——测试不得依赖宿主机的盘符布局）。

use dsh_desktop::core::tools::ToolRegistry;

#[test]
fn del_slash_f_flag_not_treated_as_path() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let reg = ToolRegistry::new(cwd.clone()).with_workspace_root(Some(cwd.clone()));
    let inside_abs = |rel: &str| cwd.join(rel).display().to_string();

    // 1) del /f /q file.txt → /f /q 是标志不是路径 → 不触发审批
    let r = reg.potential_out_of_workspace(
        "bash",
        &serde_json::json!({"command": "del /f /q test.txt"}),
    );
    assert!(r.is_none(), "del /f /q 不应触发审批: {r:?}");

    // 2) del /f /q 工作区内绝对路径 → 不触发
    let r2 = reg.potential_out_of_workspace(
        "bash",
        &serde_json::json!({"command": format!("del /f /q {}", inside_abs(r"subile.txt"))}),
    );
    assert!(r2.is_none(), "工作区内删除不应触发: {r2:?}");

    // 3) del /f /q 工作区外（C:\Windows）→ 触发（正确的审批行为）
    let r3 = reg.potential_out_of_workspace(
        "bash",
        &serde_json::json!({"command": r"del /f /q C:\Windows\system32\file.dll"}),
    );
    assert!(r3.is_some(), "工作区外删除应触发审批: {r3:?}");

    // 4) PowerShell Remove-Item -Force → -Force 是标志
    let r4 = reg.potential_out_of_workspace(
        "pwsh",
        &serde_json::json!({"command": "Remove-Item -Force -Path test.txt"}),
    );
    assert!(r4.is_none(), "Remove-Item -Force 工作区内不应触发: {r4:?}");

    // 5) copy /Y src dst → /Y 是标志，dst 在工作区内 → 不触发
    let r5 = reg.potential_out_of_workspace(
        "bash",
        &serde_json::json!({"command": format!("copy /Y source.txt {}", inside_abs("dest.txt"))}),
    );
    assert!(r5.is_none(), "copy /Y 工作区内不应触发: {r5:?}");

    eprintln!("PASS: /f /q -Force /Y 等标志不再被误认为路径");
}
