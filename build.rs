// B11（blockers-engineering.md）：把「线上跑哪个 commit」变成可机械回答的问题。
//
// 取值优先级：
// 1. 环境变量 KIRO_BUILD_SHA（部署流程显式传快照 commit 短 sha——git archive 解包的
//    Docker 构建环境没有 .git，只能靠构建时注入）。
// 2. git rev-parse --short HEAD（本地开发 / 有 .git 的构建环境）。
// 3. "unknown"（无 .git 且未传 env——Docker 构建兜底，绝不因此失败）。
//
// 产物注入方式：cargo:rustc-env → main.rs 用 env!("KIRO_BUILD_SHA") 编译期读取，
// healthz 返回 build_sha 字段、启动日志打印。
fn main() {
    let sha = std::env::var("KIRO_BUILD_SHA")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    (!s.is_empty()).then_some(s)
                })
        })
        .unwrap_or_else(|| "unknown".to_string());

    // commit 推进 / 传参变化都要重跑本脚本，否则编译期 env 陈旧。
    println!("cargo:rerun-if-env-changed=KIRO_BUILD_SHA");
    if std::path::Path::new(".git/HEAD").exists() {
        println!("cargo:rerun-if-changed=.git/HEAD");
    }
    println!("cargo:rustc-env=KIRO_BUILD_SHA={sha}");
}
