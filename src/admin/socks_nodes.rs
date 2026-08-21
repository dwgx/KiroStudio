//! 代理节点池：健康调度、落盘、CRUD。
//!
//! 由 `service.rs` 以 `#[path]` 接入。`AdminService` 仍在父文件；本文件只持 socks 簇。
//! `NodePlan` / `resolve_node_plan` 留在父文件（clone/multi-open 胶）。

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;

use crate::admin::types::{
    SocksNodeBulkImportItem, SocksNodeBulkImportOutcome, SocksNodeView,
};
pub use crate::admin::types::SocksNodeUpsertRequest;
use crate::kiro::model::socks_node::{
    MAX_SOCKS_NODES, SocksNode, SocksNodeFileCompat,
};
pub use crate::kiro::model::socks_node::SocksNodeTest;
use crate::kiro::token_manager::MultiTokenManager;

use super::AdminServiceError;

/// 代理池自动健康探测间隔（秒），固定 5 分钟。
///
/// 刻意不提供配置项：`model/config.rs` 不在改动范围，且探测节奏属运维内部策略，
/// 与 `balance_refresh_interval_secs`（面板可调）定位不同。改这里 + 重启即生效。
const SOCKS_HEALTH_CHECK_INTERVAL_SECS: u64 = 300;

/// 连续失败多少次后自动禁用该节点（对齐「连续失败 N 次」的调度语义）。
///
/// 判定在 `run_socks_health_round` 内按**连续**失败计数（成功即清零），
/// 达阈值把 `enabled` 置 false 并落盘——面板节点卡片可看到最近失败与原因。
const SOCKS_HEALTH_FAIL_THRESHOLD: u32 = 3;

impl super::AdminService {
    /// 重挂代理池自动健康调度任务（受管任务槽，对齐 [`Self::respawn_balance_task`]）。
    ///
    /// - 启动入口：由 `respawn_balance_task` 顺带调用（见其开头注释）；
    /// - 开关 `socks_auto_health` 在任务循环内自检（改开关走 update_config，
    ///   不需要重挂——关着就整轮跳过，重开即恢复探测）；
    /// - 幂等：重复调用先 abort 旧句柄再重建，不会累积多个循环；
    /// - 任务体持 `Weak<Self>`：AdminService 被 drop 后下一轮 upgrade 失败即自我退出。
    ///
    /// 间隔固定 `SOCKS_HEALTH_CHECK_INTERVAL_SECS`（无配置项，见常量注释）。
    /// 首轮等满一个完整间隔才开始（对齐余额任务，避免启动即打一批探针）。
    pub fn respawn_socks_health_task(self: &Arc<Self>) {
        let mut slot = self.socks_health_task.lock();
        if let Some(old) = slot.take() {
            old.abort();
        }
        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                SOCKS_HEALTH_CHECK_INTERVAL_SECS,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(svc) = weak.upgrade() else {
                    tracing::debug!("AdminService 已释放，代理池健康调度任务退出");
                    break;
                };
                // 开关在任务内自检：关闭时整轮跳过（任务常驻但不做事，
                // 重开无需重挂）。池空时 `run_socks_health_round` 内部直接返回。
                if !svc.socks_auto_health.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                svc.run_socks_health_round().await;
            }
        });
        *slot = Some(handle);
        tracing::info!(
            "代理池自动健康调度已启用：间隔 {} 秒，连续失败 {} 次自动禁用",
            SOCKS_HEALTH_CHECK_INTERVAL_SECS,
            SOCKS_HEALTH_FAIL_THRESHOLD
        );
    }

    /// 跑一轮代理池健康探测：对池内**启用**节点逐个探测，按连续失败计数处置。
    ///
    /// - 池空直接返回（「只在池非空时跑」）；
    /// - round-robin：每轮从不同起点开始（`socks_health_round` 取模），
    ///   保证长时间运行下各节点被探测的时机公平，不固定偏袒队首；
    /// - 节点间**不**加 sleep：探针目标是固定公共服务（非上游 kiro，无风控节奏约束），
    ///   且单节点 10s 超时本身就是天然节奏；一轮慢不会丢下一轮
    ///   （`MissedTickBehavior::Skip`，探测本身串行不并发）。
    async fn run_socks_health_round(&self) {
        let enabled: Vec<(u64, String, Option<String>, Option<String>)> = {
            let nodes = self.socks_nodes.lock();
            nodes
                .iter()
                .filter(|n| n.enabled)
                .map(|n| (n.id, n.url.clone(), n.username.clone(), n.password.clone()))
                .collect()
        };
        if enabled.is_empty() {
            return;
        }
        let start = self
            .socks_health_round
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            as usize
            % enabled.len();
        for k in 0..enabled.len() {
            let (id, url, user, pass) = &enabled[(start + k) % enabled.len()];
            let test = self
                .probe_socks_node(url, user.clone(), pass.clone())
                .await;
            self.apply_socks_health_result(*id, test);
        }
    }

    /// 探测单个代理节点（复用 `/proxy/test` 与 `/socks/nodes/{id}/test` 的探针口径）。
    ///
    /// 探针 URL 与 `handlers.rs::run_proxy_probe` 共用同一常量（SSRF 防线：
    /// 目标硬编码固定，绝不接受请求方传入）。返回 `SocksNodeTest` 而非
    /// `ProxyTestResponse`：后台调度直接消费节点表同款结构，写回零转换。
    async fn probe_socks_node(
        &self,
        proxy_url: &str,
        username: Option<String>,
        password: Option<String>,
    ) -> SocksNodeTest {
        use crate::http_client::{ProxyConfig, build_client, split_proxy_credentials};
        use crate::admin::handlers::PROXY_TEST_PROBE_URL;

        let started = std::time::Instant::now();
        let tested_at = chrono::Utc::now().timestamp().max(0) as u64;
        let fail = |error: String| SocksNodeTest {
            ok: false,
            latency_ms: started.elapsed().as_millis() as u64,
            exit_ip: None,
            error: Some(error),
            tested_at,
        };

        // 拆出干净 URL 与内嵌账密；显式字段优先覆盖内嵌账密（与 run_proxy_probe 同款）。
        let (clean_url, embedded_user, embedded_pass) = split_proxy_credentials(proxy_url);
        // 池内节点按 SSRF 校验入库，直连形态理论不存在；真出现就按失败计（无意义探测）。
        if clean_url.is_empty() || clean_url.eq_ignore_ascii_case("direct") {
            return fail("节点地址无效（直连形态，后台调度拒绝探测）".into());
        }
        let username = username.filter(|s| !s.trim().is_empty()).or(embedded_user);
        let password = password.filter(|s| !s.is_empty()).or(embedded_pass);
        let mut cfg = ProxyConfig::new(clean_url);
        if let (Some(u), Some(p)) = (username, password) {
            cfg = cfg.with_auth(u, p);
        }
        // 与 run_proxy_probe 同款 10s 超时（连不上/超时都算失败）。
        let client = match build_client(Some(&cfg), 10, self.tls_backend()) {
            Ok(c) => c,
            Err(e) => return fail(format!("构建代理客户端失败: {e}")),
        };

        // 目标固定为硬编码探针 URL（与 /proxy/test 同一常量，见该常量注释）。
        match client.get(PROXY_TEST_PROBE_URL).send().await {
            Ok(resp) => {
                let status = resp.status();
                let latency_ms = started.elapsed().as_millis() as u64;
                if !status.is_success() {
                    return fail(format!("探针返回非 2xx 状态: {status}"));
                }
                // 解析 {"ip":"..."}；解析失败不影响连通性判定，仅 exit_ip 为 None。
                let exit_ip = resp.json::<serde_json::Value>().await.ok().and_then(|v| {
                    v.get("ip")
                        .and_then(|ip| ip.as_str().map(|s| s.to_string()))
                });
                SocksNodeTest {
                    ok: true,
                    latency_ms,
                    exit_ip,
                    error: None,
                    tested_at,
                }
            }
            Err(e) => fail(format!("代理连通失败: {e}")),
        }
    }

    /// 处置一次自动探测的结果：成功清零计数并写回；失败累计，达阈值自动禁用。
    ///
    /// 锁序注意：本方法**从不**同时持有 `socks_fail_counts` 与 `socks_nodes` 两把锁
    /// （计数在短临界区内算完即释放，再单独走节点写路径），
    /// 与 `upsert_socks_node` 的「nodes 锁内查计数」方向一致，无死锁交叉。
    fn apply_socks_health_result(&self, id: u64, test: SocksNodeTest) {
        if test.ok {
            self.socks_fail_counts.lock().remove(&id);
            if let Err(e) = self.record_socks_node_test(id, test) {
                tracing::warn!("代理池健康调度：写回节点 #{id} 成功结果失败: {e}");
            }
            return;
        }
        // 失败：计数在短临界区内 +1 后立即释放 counts 锁（见方法注释的锁序说明）。
        let fails = {
            let mut m = self.socks_fail_counts.lock();
            let c = m.entry(id).or_insert(0);
            *c += 1;
            *c
        };
        if fails < SOCKS_HEALTH_FAIL_THRESHOLD {
            // 未达阈值：只写回失败结果（面板可见「最近失败」，还不到动手的时机）。
            if let Err(e) = self.record_socks_node_test(id, test) {
                tracing::warn!("代理池健康调度：写回节点 #{id} 失败结果失败: {e}");
            }
            return;
        }
        // 达阈值：自动禁用（enabled=false + 失败结果 + 计数清零 + 落盘）。
        // 禁用只改节点表本身——已绑该节点的凭据保持绑定（与手动删除同语义，
        // 不主动切走既有出口），节点只从「新分配候选」里消失。
        self.socks_fail_counts.lock().remove(&id);
        let note = format!("连续 {fails} 次探测失败，已自动禁用");
        let mut disabled_test = test;
        disabled_test.error = Some(note);
        {
            // 只读降级与 record 路径同款先判后改：拒写时内存也不动（防内存/磁盘不一致）。
            if let Err(e) = self.ensure_socks_writable() {
                tracing::warn!("代理池健康调度：节点 #{id} 已连续失败 {fails} 次，但节点表只读降级，自动禁用被跳过: {e}");
                return;
            }
            let mut nodes = self.socks_nodes.lock();
            let Some(node) = nodes.iter_mut().find(|n| n.id == id) else {
                tracing::debug!("代理池健康调度：节点 #{id} 已被删除，跳过自动禁用");
                return;
            };
            node.enabled = false;
            node.last_test = Some(disabled_test);
        }
        // ⭐ 落盘必须在节点锁**之外**：persist 内部会重新锁节点表
        // （与 upsert_socks_node 的「先 drop(nodes) 再 persist」同款，持锁调用必死锁）。
        match self.persist_socks_nodes() {
            Ok(()) => tracing::info!("代理池健康调度：节点 #{id} 连续失败 {fails} 次，已自动禁用"),
            Err(e) => tracing::warn!("代理池健康调度：节点 #{id} 自动禁用后落盘失败: {e}"),
        }
    }

    /// 从磁盘加载代理节点表。
    ///
    /// **fail-soft**：解密/解析失败一律 `warn!` + 空表，绝不 bail。
    /// 理由：at-rest 密钥是机器绑定的，换机/重建 VPS 时 credentials 那条路径是
    /// `exit(1)`（凭据没了服务本来就没意义），但节点表只是候选池 ——
    /// 不该因为它解不开就让整个网关起不来。
    /// 从磁盘加载代理节点表。返回 `(节点表, 是否可安全回写)`。
    ///
    /// **「文件缺失」与「文件在但读不出来」必须分开处理**，这是本函数唯一的要点：
    ///
    /// - 缺失 → 首次启动，空表 + 允许回写。
    /// - 在但解不开/解析失败 → 空表 + **禁止回写**。
    ///
    /// 若两者都按「空表 + 允许回写」处理，就构成一条静默数据毁灭链：启动读不出来
    /// → 内存空表 → 用户加**一个**节点 → `persist_socks_nodes` 把这张只有一条的表
    /// 原子覆盖上去 → 原文件里那 20 个节点和它们的代理密码永久消失。
    /// credentials.json 那条路径是靠 `main.rs` 直接 `exit(1)` 避免同类事故的；
    /// 节点表不该让服务起不来（它只是候选池），所以改用「只读降级」而不是退出。
    pub(super) fn load_socks_nodes_from(
        path: &Option<PathBuf>,
        token_manager: &Arc<MultiTokenManager>,
    ) -> (Vec<SocksNode>, u64, bool) {
        let path = match path {
            Some(p) => p,
            None => return (Vec::new(), 1, true),
        };
        let raw = match std::fs::read(path) {
            Ok(b) => b,
            // 文件不存在是首次启动的正常状态，不打日志，允许回写。
            Err(_) => return (Vec::new(), 1, true),
        };
        let key_path = crate::common::secret_store::key_path_for(path);
        let text = match crate::common::secret_store::maybe_decrypt_to_string(&raw, &key_path) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(
                    "代理节点表存在但解密失败，已进入**只读降级**（不会覆盖该文件）：{}。\
                     常见原因是 at-rest 密钥丢失或与数据不匹配；修好密钥后重启即恢复。",
                    e
                );
                return (Vec::new(), 1, false);
            }
        };
        match serde_json::from_str::<SocksNodeFileCompat>(&text) {
            Ok(compat) => {
                let (v, next_id) = compat.normalize();
                // 超限**不截断**：截断后第一次回写就把多出来的永久删掉。
                // 只拒绝新增（见 upsert 的上限判断），已有的照常可用。
                if v.len() > MAX_SOCKS_NODES {
                    tracing::warn!(
                        "代理节点表有 {} 条，超过上限 {}：全部保留可用，但不再允许新增",
                        v.len(),
                        MAX_SOCKS_NODES
                    );
                }
                let _ = token_manager; // 预留：将来按节点校验凭据绑定
                (v, next_id, true)
            }
            Err(e) => {
                tracing::error!(
                    "代理节点表存在但解析失败，已进入**只读降级**（不会覆盖该文件）：{}",
                    e
                );
                (Vec::new(), 1, false)
            }
        }
    }

    /// 只读降级检查，**必须在改内存之前调用**。
    ///
    /// 为什么不能只靠 `persist_socks_nodes` 那道判断：那道判断在**改完内存之后**才跑，
    /// 于是只读降级下的一次 upsert 会「内存里真的多出一个节点 + 调用方收到报错」——
    /// 面板列表从此显示一个磁盘上并不存在的节点，直到重启才消失，
    /// 而用户看到的是「保存失败但它出现了」，只会以为报错是假的、节点是真的。
    /// 三个写入方法（upsert / delete / record_test）都在顶部调它，先判后改。
    fn ensure_socks_writable(&self) -> Result<(), AdminServiceError> {
        // path 为 None 是纯内存态（单凭据格式），此时 writable 恒 true，与 persist 同口径。
        if self.socks_nodes_path.is_some() && !self.socks_nodes_writable {
            return Err(AdminServiceError::InternalError(
                "代理节点表处于只读降级（启动时该文件解密/解析失败）：\
                 为避免覆盖原文件，本次修改未落盘。请修复 at-rest 密钥后重启。"
                    .into(),
            ));
        }
        Ok(())
    }

    /// 回写代理节点表（含密码，故与 credentials/trash 同开关同密钥做 at-rest 加密）。
    ///
    /// 两条护栏：
    /// 1. **只读降级时拒绝写**（`socks_nodes_writable=false`）—— 启动时文件读不出来，
    ///    内存里是空表，写下去就等于把原文件抹平。
    /// 2. **序列化与写盘在同一把锁内**：先前把序列化放锁内、写盘放锁外，两个并发
    ///    修改会各自持有一份快照，后完成的那次写把先完成的改动覆盖掉（丢写）。
    pub(super) fn persist_socks_nodes(&self) -> Result<(), AdminServiceError> {
        let path = match &self.socks_nodes_path {
            Some(p) => p,
            // 单凭据格式：纯内存态（与 trash 同款约定）。
            None => return Ok(()),
        };
        if !self.socks_nodes_writable {
            return Err(AdminServiceError::InternalError(
                "代理节点表处于只读降级（启动时该文件解密/解析失败）：\
                 为避免覆盖原文件，本次修改未落盘。请修复 at-rest 密钥后重启。"
                    .into(),
            ));
        }
        let enc = self.token_manager.config().encrypt_credentials_at_rest;
        let key_path = crate::common::secret_store::key_path_for(path);
        // ⭐ 整段在锁内：序列化 → 编码 → 原子写。放开锁再写会丢写（见上）。
        let nodes = self.socks_nodes.lock();
        let file = crate::kiro::model::socks_node::SocksNodeFile {
            nodes: nodes.clone(),
            next_id: self
                .socks_next_id
                .load(std::sync::atomic::Ordering::Relaxed),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| AdminServiceError::InternalError(format!("序列化节点表失败: {e}")))?;
        let (bytes, encrypted) =
            crate::common::secret_store::encode_for_disk(json.as_bytes(), enc, &key_path);
        // 与 token_manager 的 persist_credentials 同口径：开了加密却落成明文时
        // 必须把面板的 at-rest 健康灯打灭，否则密码明文落盘而界面显示一切正常。
        crate::common::recovery_metrics::set_at_rest_healthy(!enc || encrypted);
        crate::common::fs_atomic::write_atomic(path, &bytes)
            .map_err(|e| AdminServiceError::InternalError(format!("回写节点表失败: {e}")))?;
        Ok(())
    }

    /// 列出所有代理节点。**密码恒不外传**，只给 `hasPassword` 布尔。
    ///
    /// 同时带上「这个节点上已挂了几个凭据」（`boundCredentials`）：前端的节点下拉与
    /// 「自动分配」按钮按它排序，必须与 `resolve_node_plan` 的自动分配同一口径，
    /// 否则推荐顺序与实际分配结果不一致。计数表一次算好复用给全部节点（O(凭据数)），
    /// 且在 `socks_nodes` 锁**之外**取（避免与 token_manager.entries 构成新锁序）。
    pub fn list_socks_nodes(&self) -> Vec<SocksNodeView> {
        let usage = self.token_manager.proxy_url_usage();
        self.socks_nodes
            .lock()
            .iter()
            .map(|n| SocksNodeView::from_node(n, usage.get(&n.url).copied().unwrap_or(0)))
            .collect()
    }

    /// 批量导入代理节点（整段粘贴节点商文档）。
    ///
    /// 返回四个聚合计数 + **逐行结果**（见 [`SocksNodeBulkImportOutcome`]）。
    ///
    /// # 为什么要逐行结果
    ///
    /// 原先只返回四个数，其中「跳过数」= 非链接行 + SSRF 拒绝 —— 用户看到「跳过 10 行」
    /// 时无法区分「这行不是链接」「这行端口写错了」「这行地址是内网被拦了」，
    /// 三者需要的动作完全不同。逐行结果让每一行都带上行号、脱敏原文和原因码。
    ///
    /// # 设计取舍
    ///
    /// - **默认不启用**（`enabled` 由调用方给，前端默认 false）：新导入的节点还没测活，
    ///   直接参与分配会把未验证的出口塞给分身。与「生成分身时是否全部默认启用」同一原则。
    /// - **URL 去重**：同一节点在节点商文档里会出现两次（整段区 + 明细区）。
    ///   已在表里的 url 直接跳过，**不覆盖**已有节点的账密/启用状态 ——
    ///   覆盖会把一个已测活启用的节点重置成未启用。
    /// - **SSRF 校验逐条做**，任一条不过只跳过它，不让整批失败
    ///   （用户粘的是一大段，为一行内网地址废掉整批很难用）。
    pub async fn bulk_import_socks_nodes(
        &self,
        text: &str,
        enabled: bool,
    ) -> Result<SocksNodeBulkImportOutcome, AdminServiceError> {
        self.ensure_socks_writable()?;
        let report = crate::http_client::parse_proxy_lines_report(text);
        let has_parsable = report.items.iter().any(|i| i.link.is_some());
        if !has_parsable {
            // 一条都解析不出来时仍报错（保持既有行为：前端据此弹 error toast）。
            // 但把**失败原因**带上 —— 原先只说「跳过 N 行非链接文本」，
            // 而真实原因常常是端口写错或格式判不定。
            let why = report
                .items
                .iter()
                .filter_map(|i| i.issue.map(|e| format!("第 {} 行 {}", i.lineno, e.code())))
                .take(5)
                .collect::<Vec<_>>()
                .join("；");
            let tail = if why.is_empty() {
                String::new()
            } else {
                format!("。可疑行：{why}")
            };
            return Err(AdminServiceError::InvalidCredential(format!(
                "没有解析出任何节点（跳过 {} 行非链接文本）。\
                 期望形如 socks://<base64 或 user:pass>@host:port#名字，\
                 或 host:port:user:pass{tail}",
                report.skipped
            )));
        }

        let mut added = 0usize;
        let mut dup = 0usize;
        let mut over_cap = 0usize;
        let mut rejected = 0usize;
        let mut items: Vec<SocksNodeBulkImportItem> = Vec::with_capacity(report.items.len());

        for it in report.items {
            let lineno = it.lineno;
            let raw = it.raw;
            let p = match it.link {
                Some(p) => p,
                None => {
                    // 解析失败：原因码原样带回（前端做 i18n 映射）。
                    let code = it
                        .issue
                        .map(|e| e.code().to_string())
                        .unwrap_or_else(|| "invalid".to_string());
                    items.push(SocksNodeBulkImportItem {
                        lineno,
                        raw,
                        status: "invalid".into(),
                        reason: Some(code),
                        address: None,
                        username: None,
                    });
                    continue;
                }
            };
            let address = Some(p.url.clone());
            let username = p.username.clone();
            // 同一次粘贴内重复：与「已在池中」同样算 duplicate（对用户是同一件事）。
            if it.dup_in_paste {
                dup += 1;
                items.push(SocksNodeBulkImportItem {
                    lineno,
                    raw,
                    status: "duplicate".into(),
                    reason: Some("dup_in_paste".into()),
                    address,
                    username,
                });
                continue;
            }
            // 已存在（按 url）→ 跳过，绝不覆盖既有节点的账密/启用状态。
            if self.socks_nodes.lock().iter().any(|n| n.url == p.url) {
                dup += 1;
                items.push(SocksNodeBulkImportItem {
                    lineno,
                    raw,
                    status: "duplicate".into(),
                    reason: Some("already_in_pool".into()),
                    address,
                    username,
                });
                continue;
            }
            // SSRF：逐条校验，不过则只跳过这一条（await 必须在锁外）。
            if let Err(e) = crate::common::ssrf::validate_proxy_address(&p.url).await {
                // 只跳过这一条并告警：用户粘的是一大段，为一行内网地址废掉整批很难用。
                tracing::warn!("批量导入跳过节点 {}（地址校验未通过）: {}", p.url, e);
                rejected += 1;
                items.push(SocksNodeBulkImportItem {
                    lineno,
                    raw,
                    status: "invalid".into(),
                    // 与解析失败区分开：地址本身合法，是**策略**拦下的。
                    reason: Some("address_rejected".into()),
                    address,
                    username,
                });
                continue;
            }
            let mut nodes = self.socks_nodes.lock();
            if nodes.len() >= MAX_SOCKS_NODES {
                over_cap += 1;
                items.push(SocksNodeBulkImportItem {
                    lineno,
                    raw,
                    status: "over_capacity".into(),
                    reason: Some("over_capacity".into()),
                    address,
                    username,
                });
                continue;
            }
            let id = self
                .socks_next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            nodes.push(SocksNode {
                id,
                name: p.name.clone().unwrap_or_default(),
                url: p.url.clone(),
                username: p.username.clone(),
                password: p.password.clone(),
                enabled,
                last_test: None,
                created_at: Utc::now().timestamp().max(0) as u64,
            });
            added += 1;
            items.push(SocksNodeBulkImportItem {
                lineno,
                raw,
                status: "ok".into(),
                reason: None,
                address,
                username,
            });
        }

        if added > 0 {
            self.persist_socks_nodes()?;
        }
        Ok(SocksNodeBulkImportOutcome {
            added,
            // 保持旧口径：非链接行 + SSRF 拒绝。含义比字面宽，
            // 精确归因看 `items`（这正是加它的理由）。
            skipped: report.skipped + rejected,
            duplicate: dup,
            over_capacity: over_cap,
            items,
        })
    }

    /// 新建或更新一个代理节点。
    ///
    /// `id = None` → 新建；`Some(existing)` → 更新；`Some(不存在)` → NotFound
    /// （**不静默新建**：那会把一次误传的 id 变成一个用户没预期的新节点）。
    pub async fn upsert_socks_node(
        &self,
        req: SocksNodeUpsertRequest,
    ) -> Result<u64, AdminServiceError> {
        // ⭐ 先判只读降级再改内存：否则内存表会多出一个磁盘上不存在的节点（见 ensure_socks_writable）。
        self.ensure_socks_writable()?;
        // 账密从 URL 里拆出来，避免密码明文留在 url 字段里（与 set_credential_proxy 同口径）。
        let raw = req.url.trim();
        if raw.is_empty() {
            return Err(AdminServiceError::InvalidCredential("url 不能为空".into()));
        }
        // ⭐ 先试**分享链接**解析（`socks://base64(user:pass)@host:port#name`）——
        // 机场/节点商下发的就是这个形式，而 `split_proxy_credentials` 只做百分号解码，
        // 会把整个 base64 串当成用户名、密码为 None ⇒ 代理认证必然失败，
        // 而那个失败长得像「节点不通」，会把排障带偏。`#name` 还会残留在 URL 里污染 host。
        //
        // 解析不出（普通 `socks5://host:port` 或已拆好账密的表单提交）时回落原路径，
        // 行为逐字不变。
        let (clean_url, inline_user, inline_pass, link_name) =
            match crate::http_client::parse_proxy_link(raw) {
                Some(p) => (p.url, p.username, p.password, p.name),
                None => {
                    let (u, iu, ip) = crate::http_client::split_proxy_credentials(raw);
                    (u, iu, ip, None)
                }
            };

        // 拦内网/环回：节点地址会被写进凭据并在热路径上使用。
        // 策略是 SsrfPolicy::AdminConfigured（与 custom_api base_url 同口径）：管理员亲手填的
        // 目标，只放开 198.18.0.0/15 那一段 —— 那是 Clash/Mihomo 的 fake-IP 池默认段，
        // 用 Strict 会让开了 fake-IP 的机器一个域名形式的节点都加不进来。
        // ⚠️ 环回与 RFC1918 **仍然被拒**（本机 ssh -D 隧道 / 局域网旁车加不进来），
        // 这是当前的已知限制，不是 AdminConfigured 能解决的 —— 见 validate_proxy_address 文档。
        // ⚠️ 这**不是**安全边界（DNS 失败放行、不在使用时复验、且 set_credential_proxy
        // 与 /proxy/test 两条旁路完全不校验）—— 见 validate_proxy_address 的文档。
        crate::common::ssrf::validate_proxy_address(&clean_url)
            .await
            .map_err(AdminServiceError::InvalidCredential)?;

        let username = req
            .username
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| inline_user.clone());

        let mut nodes = self.socks_nodes.lock();

        let id = match req.id {
            Some(id) => {
                let node = nodes
                    .iter_mut()
                    .find(|n| n.id == id)
                    .ok_or(AdminServiceError::NotFound { id })?;
                // 名字优先级：显式 req.name > 分享链接的 #fragment > 保持原值。
                node.name = req
                    .name
                    .clone()
                    .or_else(|| link_name.clone())
                    .unwrap_or_else(|| node.name.clone());
                node.url = clean_url;
                // ⭐ 分享链接自带账密时，即使 req.username/password 都省略也要写入 ——
                // 编辑场景下用户粘一条新链接进来，期望的是"整条替换"，而三态语义
                // （省略=不改）会让新链接的账密被丢弃、继续用旧的 ⇒ 认证失败。
                if req.username.is_none() {
                    if let Some(u) = inline_user.clone() {
                        node.username = Some(u);
                    }
                }
                if req.password.is_none() {
                    if let Some(p) = inline_pass.clone() {
                        node.password = Some(p);
                    }
                }
                // 用户名与密码同款三态：**省略该键 = 不改**，`Some("") = 清空`。
                // 先前这里是无条件赋值，于是只发 {id,url,enabled} 的更新会把用户名
                // 抹成 None 而密码留着 → `build_client` 的 `if let (Some(u), Some(p))`
                // 不成立 → 认证被静默丢弃 → 该节点此后全部连不上。
                match req.username.as_ref() {
                    None => {}
                    Some(u) if u.is_empty() => node.username = None,
                    Some(_) => node.username = username,
                }
                // ⭐ 密码语义：**省略该键 = 不改**，`Some("") = 清空`。
                // 绝不能写成必填 —— 那样「改个节点名」就会把密码抹掉，
                // 已绑该节点的分身全部掉线（GET 抹密码 + 前端整体回填的经典坑）。
                match req.password.as_ref() {
                    None => {}
                    Some(p) if p.is_empty() => node.password = None,
                    Some(p) => node.password = Some(p.clone()),
                }
                if let Some(en) = req.enabled {
                    node.enabled = en;
                }
                // 手动启用即视为「已人工确认恢复」：清零自动健康调度的失败计数，
                // 否则刚手动拉起的节点会背着历史失败数、下轮探测失败一次就被禁。
                // 计数只存活于内存（见 `socks_fail_counts` 字段注释），此处仅做 remove。
                if req.enabled == Some(true) {
                    self.socks_fail_counts.lock().remove(&id);
                }
                id
            }
            None => {
                if nodes.len() >= MAX_SOCKS_NODES {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "节点数已达上限 {MAX_SOCKS_NODES}"
                    )));
                }
                // id 从持久化高水位取，**不用** `max(现有 id)+1` —— 后者在删掉
                // 最大 id 的节点后会把该 id 重新发出去，让仍持有旧列表的面板标签页
                // 指向一个无关新节点。
                let id = self
                    .socks_next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                nodes.push(SocksNode {
                    id,
                    // 名字优先级：显式 req.name > 分享链接的 #fragment > 空（前端回落 host:port）。
                    // 用 #fragment 当名字是刻意的：粘一条链接进来就能得到「US-1-SOCKS5」
                    // 这种可读标签，而不是一列长得一样的 IP。
                    name: req
                        .name
                        .clone()
                        .or_else(|| link_name.clone())
                        .unwrap_or_default(),
                    url: clean_url,
                    username,
                    password: req
                        .password
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or(inline_pass),
                    enabled: req.enabled.unwrap_or(true),
                    last_test: None,
                    created_at: Utc::now().timestamp().max(0) as u64,
                });
                id
            }
        };
        drop(nodes);
        self.persist_socks_nodes()?;
        Ok(id)
    }

    /// 删除一个代理节点。**不动已绑该节点的凭据** —— 凭据的 `proxy_*` 是独立的绑定
    /// 结果，删节点只是把它从候选池移除；否则删一个节点会让一批分身当场掉线。
    pub fn delete_socks_node(&self, id: u64) -> Result<bool, AdminServiceError> {
        // ⭐ 先判后改：只读降级下删除若先动内存，节点会从面板消失但磁盘上还在。
        self.ensure_socks_writable()?;
        let removed = {
            let mut nodes = self.socks_nodes.lock();
            let before = nodes.len();
            nodes.retain(|n| n.id != id);
            before != nodes.len()
        };
        if removed {
            self.persist_socks_nodes()?;
        }
        Ok(removed)
    }

    /// 写回某节点的测速结果（由 `/socks/nodes/{id}/test` 调用）。
    pub fn record_socks_node_test(
        &self,
        id: u64,
        test: SocksNodeTest,
    ) -> Result<(), AdminServiceError> {
        // ⭐ 先判后改：只读降级下写测速结果若先动内存，面板会显示一个不会被持久化的结果。
        self.ensure_socks_writable()?;
        {
            let mut nodes = self.socks_nodes.lock();
            let node = nodes
                .iter_mut()
                .find(|n| n.id == id)
                .ok_or(AdminServiceError::NotFound { id })?;
            node.last_test = Some(test);
        }
        self.persist_socks_nodes()
    }

    /// 取某节点的完整代理配置（含密码），供测速与「一键生成分身」使用。
    pub fn socks_node_proxy(&self, id: u64) -> Option<(String, Option<String>, Option<String>)> {
        self.socks_nodes
            .lock()
            .iter()
            .find(|n| n.id == id)
            .map(|n| (n.url.clone(), n.username.clone(), n.password.clone()))
    }
}
