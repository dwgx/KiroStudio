//! 配置热更：PUT /config 的 load → 改字段 → save → reload / setter。
//!
//! 由 `service.rs` 以 `#[path]` 接入。`AdminService` 仍在父文件；本文件只持
//! `update_config` / `update_config_locked`。`import_config` / `export_config` /
//! `restart_service` / `validate_error_messages` 留在父文件。

use std::sync::Arc;

use crate::admin::types::{UpdateConfigRequest, UpdateConfigResponse};

use super::AdminServiceError;
use super::{diff_json_fields, rotate_config_backup, validate_error_messages};

impl super::AdminService {
    /// 更新服务端配置并持久化到 config.json
    ///
    /// # 并发写锁（2026-08-14）
    ///
    /// 本方法包住「load → 逐字段改 → save → reload_config」整段（见
    /// `update_config_locked`）。并发两个 PUT /config 时，若各自 load 后交错 save，
    /// 后完成者会把先完成者的改动整体覆盖（lost update）。持锁串行后互不覆盖。
    /// 锁内无任何 await（本函数与内部全部是同步调用），`parking_lot::Mutex` 足够。
    pub fn update_config(
        self: &Arc<Self>,
        req: UpdateConfigRequest,
    ) -> Result<UpdateConfigResponse, AdminServiceError> {
        let _guard = self.config_write_lock.lock();
        self.update_config_locked(req)
    }

    /// `update_config` 的锁内实现（原函数体）。**只有** `update_config` 包装函数
    /// 与 `import_config` 会调用它，调用方必须先持 `config_write_lock`。
    pub(super) fn update_config_locked(
        self: &Arc<Self>,
        req: UpdateConfigRequest,
    ) -> Result<UpdateConfigResponse, AdminServiceError> {
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                AdminServiceError::InternalError("配置文件路径未知，无法保存配置".to_string())
            })?;

        // 从磁盘重新加载，避免覆盖进程外的改动
        let mut config = crate::model::config::Config::load(&config_path)
            .map_err(|e| AdminServiceError::InternalError(format!("加载配置失败: {}", e)))?;

        // 审计（2026-08-14）：保存前对比新旧 JSON，记录「变了哪些字段」——只记字段名不记值。
        let old_json = serde_json::to_value(&config).unwrap_or_default();

        let mut restart_fields: Vec<String> = Vec::new();
        // TIER1 运行时字段是否有变更 → save 后统一 reload_config 热应用（不重启即生效）。
        let mut hot_changed = false;
        // TIER2 后台任务字段是否有变更 → save+reload 后 respawn 对应任务（不重启即生效）。
        let mut refresh_task_changed = false;
        let mut balance_task_changed = false;
        // TIER3 AppState 热更字段：extract_thinking 改后调 handlers setter 即时生效（不重启）。
        let mut extract_thinking_changed: Option<bool> = None;
        // CC 自动切缓冲开关：改后调 handlers setter 即时生效（进程级镜像，不重启）。
        let mut cc_auto_buffer_changed: Option<bool> = None;
        // 推号开关无 TIER3 镜像，只需让 reload_config 跑一次即可生效，故用 bool 而非 Option<bool>。
        let mut import_keys_enabled_changed = false;
        // 分身默认启用同款：`clone_default_enabled()` 每次直接读 config ArcSwap，
        // 所以只要 reload_config 被触发就即时生效。**必须**进 hot_or_display_changed 的
        // OR 链，漏掉就是"存了盘但 ArcSwap 仍是旧值"，面板开关静默无效。
        let mut clone_default_enabled_changed = false;
        // 上游 429 吸收层十项是否有变更。**无 TIER3 setter**：吸收层在 provider 内直接读
        // token_manager 的 config ArcSwap，所以只要下面的 reload_config 被触发就即时生效。
        // ⚠️ 正因如此，这个 flag **必须**进 hot_or_display_changed 的 OR 链 ——
        // 漏掉就会「存了盘但 ArcSwap 仍是旧值」，面板开关静默无效。
        let mut absorb_changed = false;
        // 全池自愈退避参数（2026-08-11 配置化）：token_manager 每周期从 config 读，
        // 必须进 hot_or_display_changed 的 OR 链，否则「存了盘但 ArcSwap 仍是旧值」。
        let mut self_heal_changed = false;
        let mut prompt_cache_enabled_changed: Option<bool> = None;
        // 透传模拟缓存（TIER3）：enabled/ratio 任一变更都调 handlers setter 即时生效。
        let mut mock_cache_changed = false;
        // 错误码/提示词覆盖表（TIER1）：无 TIER3 setter——消费点每请求读 config
        // ArcSwap 快照查表（model_mapping 同款范式），**只**靠下面 OR 链触发
        // reload_config。漏掉这行 → 存了盘但 ArcSwap 仍是旧值，面板改完当次不生效。
        let mut error_messages_changed = false;
        // 环境噪音剥离开关：改后调 converter setter 即时生效（进程级镜像，不重启）。
        let mut strip_env_noise_changed: Option<bool> = None;
        // Kiro 原生 effort 开关：改后调 converter setter 即时生效（进程级镜像，不重启）。
        let mut native_thinking_effort_enabled_changed: Option<bool> = None;
        // CC↔Kiro 工具名/参数映射开关：改后调 converter setter 即时生效（进程级镜像，不重启）。
        let mut tool_compat_mapping_changed: Option<bool> = None;
        // 工具错误缓解三开关：改后调 handlers setter 即时生效（进程级镜像，不重启）。
        let mut tool_clean_leaked_tokens_changed: Option<bool> = None;
        let mut tool_stream_align_failure_changed: Option<bool> = None;
        let mut tool_expose_error_to_client_changed: Option<bool> = None;
        let mut tool_repair_json_changed: Option<bool> = None;
        let mut tool_truncation_recovery_changed: Option<bool> = None;
        let mut tool_description_max_chars_changed: Option<usize> = None;
        // at-rest 加密开关变更:变更后立即重写凭据/回收站文件(明文↔密文),不等下次偶发变更。
        let mut encrypt_at_rest_changed = false;
        // 两把鉴权 key 的轮换：存盘后调 auth_keys setter 即时生效（不再进 restart_fields）。
        // 存 trim 后的新值而非 bool——setter 需要实际值，且 reload_config 会把 config 里的
        // 这两把 key 钉回启动值（restart-only 字段的 split-brain 防护），故热更单元是它们
        // 唯一的活真相源（详见下方 setter 调用处的顺序注释）。
        let mut user_key_changed: Option<String> = None;
        let mut admin_key_changed: Option<String> = None;

        // —— 需重启生效的字段 ——
        if let Some(v) = req.host {
            let v = v.trim().to_string();
            if v.is_empty() {
                return Err(AdminServiceError::InvalidCredential(
                    "host 不能为空".to_string(),
                ));
            }
            if v != config.host {
                config.host = v;
                restart_fields.push("host".into());
            }
        }
        if let Some(v) = req.port {
            if v == 0 {
                return Err(AdminServiceError::InvalidCredential(
                    "port 必须是 1-65535".to_string(),
                ));
            }
            if v != config.port {
                config.port = v;
                restart_fields.push("port".into());
            }
        }
        if let Some(v) = req.region {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.region {
                config.region = v;
                restart_fields.push("region".into());
            }
        }
        if let Some(v) = req.kiro_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.kiro_version {
                config.kiro_version = v;
                restart_fields.push("kiroVersion".into());
            }
        }
        if let Some(v) = req.system_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.system_version {
                config.system_version = v;
                restart_fields.push("systemVersion".into());
            }
        }
        if let Some(v) = req.node_version {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.node_version {
                config.node_version = v;
                restart_fields.push("nodeVersion".into());
            }
        }
        if let Some(v) = req.tls_backend {
            // 出厂发布版一律纯 rustls（见 build.bat / release.yml 的 --no-default-features）。
            // native-tls 已是死路：前端已移除该选项，此处对任何非 rustls 值一律归一到 rustls，
            // 避免把一个"点了会触发回退警告"的死后端持久化进 config.json。宽容接收旧客户端/
            // 旧脚本传来的 "native-tls"，静默归一而非报错（防呆）。
            let backend = match v.as_str() {
                "native-tls" => {
                    tracing::warn!("tlsBackend=native-tls 已废弃，自动归一到 rustls（功能等价）");
                    crate::model::config::TlsBackend::Rustls
                }
                _ => crate::model::config::TlsBackend::Rustls,
            };
            if backend != config.tls_backend {
                config.tls_backend = backend;
                restart_fields.push("tlsBackend".into());
            }
        }
        if let Some(v) = req.default_endpoint {
            let v = v.trim().to_string();
            if !v.is_empty() && v != config.default_endpoint {
                if !self.known_endpoints.is_empty() && !self.known_endpoints.contains(&v) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "未知 endpoint '{}'，可用: {}",
                        v,
                        {
                            let mut names: Vec<_> = self.known_endpoints.iter().cloned().collect();
                            names.sort();
                            names.join(", ")
                        }
                    )));
                }
                config.default_endpoint = v;
                restart_fields.push("defaultEndpoint".into());
            }
        }
        // —— OTA 自动检查开关（需重启生效）——
        // main.rs 启动期按 config.ota_auto_check 门控 spawn 后台检查任务（无 respawn
        // 机制，TIER2 覆盖范围外），改后必须重启进程才生效 → 只进 restart_fields。
        if let Some(v) = req.ota_auto_check {
            if v != config.ota_auto_check {
                config.ota_auto_check = v;
                restart_fields.push("otaAutoCheck".into());
            }
        }
        // —— 提取 thinking 开关（TIER3 AppState 热更：改后调 handlers setter 即时生效不重启）——
        if let Some(v) = req.extract_thinking {
            if v != config.extract_thinking {
                config.extract_thinking = v;
                extract_thinking_changed = Some(v);
            }
        }
        // —— CC 自动切缓冲开关（TIER3 热更：改后调 handlers setter 即时生效不重启）——
        if let Some(v) = req.cc_auto_buffer {
            if v != config.cc_auto_buffer {
                config.cc_auto_buffer = v;
                cc_auto_buffer_changed = Some(v);
            }
        }
        // —— 批量推号入口开关（无 TIER3 setter：handler 每次直接读 config()，
        //    存盘 + reload_config 换入 ArcSwap 后下一个请求即生效）——
        if let Some(v) = req.import_keys_enabled {
            if v != config.import_keys_enabled {
                config.import_keys_enabled = v;
                import_keys_enabled_changed = true;
            }
        }
        // —— 分身默认启用（同上：无 TIER3 setter，靠 reload_config 换入 ArcSwap）——
        if let Some(v) = req.clone_default_enabled {
            if v != config.clone_default_enabled {
                config.clone_default_enabled = v;
                clone_default_enabled_changed = true;
            }
        }
        // —— 上游 429 吸收层十项（存盘 + reload_config 即时生效，无 TIER3 setter）——
        if let Some(v) = req.upstream_retry_absorb_enabled {
            if v != config.upstream_retry_absorb_enabled {
                config.upstream_retry_absorb_enabled = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_budget_secs {
            if v != config.upstream_retry_absorb_budget_secs {
                config.upstream_retry_absorb_budget_secs = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_max_rounds {
            if v != config.upstream_retry_absorb_max_rounds {
                config.upstream_retry_absorb_max_rounds = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_min_delay_ms {
            if v != config.upstream_retry_absorb_min_delay_ms {
                config.upstream_retry_absorb_min_delay_ms = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_max_delay_secs {
            if v != config.upstream_retry_absorb_max_delay_secs {
                config.upstream_retry_absorb_max_delay_secs = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_suspended {
            if v != config.upstream_retry_absorb_suspended {
                config.upstream_retry_absorb_suspended = v;
                absorb_changed = true;
            }
        }
        // 是否吸收上游 5xx（2026-08-10 补：此前该字段只能改 config.json + 重启）。
        // 线上代挂上游主要故障形态是 502，不吸收等于把最典型的瞬态故障直接甩给客户端。
        if let Some(v) = req.upstream_retry_absorb_server_error {
            if v != config.upstream_retry_absorb_server_error {
                config.upstream_retry_absorb_server_error = v;
                absorb_changed = true;
            }
        }
        // 吸收 400 容量类 / 换号空窗独立预算 / 耗尽状态码（2026-08-11 补：此前只能改 config.json）。
        if let Some(v) = req.upstream_retry_absorb_capacity_400 {
            if v != config.upstream_retry_absorb_capacity_400 {
                config.upstream_retry_absorb_capacity_400 = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_swap_budget_secs {
            if v != config.upstream_retry_absorb_swap_budget_secs {
                config.upstream_retry_absorb_swap_budget_secs = v;
                absorb_changed = true;
            }
        }
        if let Some(v) = req.upstream_retry_absorb_exhausted_status {
            if v != config.upstream_retry_absorb_exhausted_status {
                // 值域白名单（2026-08-11 审计）：config 文档明确「唯一另一个可选值 503」。
                // 消费端（provider.rs）只认精确 503、其余一律按 429 语义处理（有守卫钉死），
                // 但面板不该允许把 0/999 之类写进 config.json 长期驻留。
                if v != 429 && v != 503 {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "upstreamRetryAbsorbExhaustedStatus 只允许 429 或 503，收到 {v}"
                    )));
                }
                config.upstream_retry_absorb_exhausted_status = v;
                absorb_changed = true;
            }
        }

        // —— prompt cache 记账下发开关（TIER3 热更：改后调 handlers setter 即时生效不重启）——
        // 此前该配置既无读取点也不在 admin 请求里，等于面板改不了、改了也没用。
        if let Some(v) = req.prompt_cache_enabled {
            if v != config.prompt_cache_enabled {
                config.prompt_cache_enabled = v;
                prompt_cache_enabled_changed = Some(v);
            }
        }
        // —— 透传模拟缓存（TIER3 热更：改后调 handlers setter 即时生效不重启）——
        // 两个字段共用一个 changed 标志：任一变更是同一个 setter 调用。
        if let Some(v) = req.mock_cache_enabled {
            if v != config.mock_cache_enabled {
                config.mock_cache_enabled = v;
                mock_cache_changed = true;
            }
        }
        if let Some(v) = req.mock_cache_read_ratio {
            // 先清洗再比较/写盘：setter（handlers）侧也会清洗，但 config 结构里存
            // 原始非法值（NaN/±inf/越界）会让面板快照（读 config 结构）与热路径
            // 生效值（经 setter clamp）不一致。
            let v = crate::anthropic::handlers::sanitize_mock_cache_ratio(v);
            if v != config.mock_cache_read_ratio {
                config.mock_cache_read_ratio = v;
                mock_cache_changed = true;
            }
        }
        // —— 环境噪音剥离开关（改后调 converter setter 即时生效不重启）——
        if let Some(v) = req.strip_env_noise {
            if v != config.strip_env_noise {
                config.strip_env_noise = v;
                strip_env_noise_changed = Some(v);
            }
        }
        // —— Kiro 原生 effort 开关（改后调 converter setter 即时生效不重启）——
        if let Some(v) = req.native_thinking_effort_enabled {
            if v != config.native_thinking_effort_enabled {
                config.native_thinking_effort_enabled = v;
                native_thinking_effort_enabled_changed = Some(v);
            }
        }
        // —— CC↔Kiro 工具名/参数映射开关（改后调 converter setter 即时生效不重启）——
        if let Some(v) = req.tool_compat_mapping {
            if v != config.tool_compat_mapping {
                config.tool_compat_mapping = v;
                tool_compat_mapping_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_clean_leaked_tokens {
            if v != config.tool_clean_leaked_tokens {
                config.tool_clean_leaked_tokens = v;
                tool_clean_leaked_tokens_changed = Some(v);
            }
        }
        // 全池自愈退避参数（2026-08-11 配置化）：无 TIER3 setter（token_manager 每周期
        // 从 config 读），改后下一个自愈周期即生效（热更语义见 config.rs 字段注释）。
        if let Some(v) = req.self_heal_base_backoff_secs {
            if v != config.self_heal_base_backoff_secs {
                config.self_heal_base_backoff_secs = v;
                self_heal_changed = true;
            }
        }
        if let Some(v) = req.self_heal_max_backoff_secs {
            if v != config.self_heal_max_backoff_secs {
                config.self_heal_max_backoff_secs = v;
                self_heal_changed = true;
            }
        }
        if let Some(v) = req.self_heal_max_shift {
            if v != config.self_heal_max_shift {
                config.self_heal_max_shift = v;
                self_heal_changed = true;
            }
        }
        if let Some(v) = req.tool_reclaim_textified_invoke {
            if v != config.tool_reclaim_textified_invoke {
                config.tool_reclaim_textified_invoke = v;
                crate::anthropic::handlers::set_tool_reclaim_textified_invoke(v);
                hot_changed = true;
            }
        }
        if let Some(v) = req.tool_stray_repeat_guard {
            if v != config.tool_stray_repeat_guard {
                config.tool_stray_repeat_guard = v;
                crate::anthropic::handlers::set_tool_stray_repeat_guard(v);
                hot_changed = true;
            }
        }
        if let Some(v) = req.tool_stream_align_failure {
            if v != config.tool_stream_align_failure {
                config.tool_stream_align_failure = v;
                tool_stream_align_failure_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_expose_error_to_client {
            if v != config.tool_expose_error_to_client {
                config.tool_expose_error_to_client = v;
                tool_expose_error_to_client_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_repair_json {
            if v != config.tool_repair_json {
                config.tool_repair_json = v;
                tool_repair_json_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_truncation_recovery {
            if v != config.tool_truncation_recovery {
                config.tool_truncation_recovery = v;
                tool_truncation_recovery_changed = Some(v);
            }
        }
        if let Some(v) = req.tool_description_max_chars {
            if v != config.tool_description_max_chars {
                config.tool_description_max_chars = v;
                tool_description_max_chars_changed = Some(v);
            }
        }
        // ── CLI 端点协议/指纹三开关 ──
        // 都**不需要** TIER3 原子镜像：`decorate_api` / `transform_api_body` 从
        // `ctx.config` 读，而那份 Config 是 provider 每次调用时 `token_manager.config()`
        // （ArcSwap `load_full`）取的新快照 ⇒ 存盘 + reload_config 后下一个请求即生效。
        // 加镜像反而多一份要同步的真值（与吸收层同理，见 provider.rs 的 AbsorbPolicy 说明）。
        // 故这里只置 `hot_changed`，不进 restart_fields、不调任何 setter。
        if let Some(v) = req.cli_origin_kiro_cli {
            if v != config.cli_origin_kiro_cli {
                config.cli_origin_kiro_cli = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.cli_codewhisperer_optout_false {
            if v != config.cli_codewhisperer_optout_false {
                config.cli_codewhisperer_optout_false = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.cli_ua_align_real_client {
            if v != config.cli_ua_align_real_client {
                config.cli_ua_align_real_client = v;
                hot_changed = true;
            }
        }
        // at-rest 加密开关:热更(persist 每次读 self.config() 现值)。开→关或关→开都在下次 persist 生效;
        // 想立即把已有明文转密文(或密文转明文),改完开关后触发任意一次凭据变更(或下方主动 persist)即可。
        if let Some(v) = req.encrypt_credentials_at_rest {
            if v != config.encrypt_credentials_at_rest {
                config.encrypt_credentials_at_rest = v;
                hot_changed = true;
                encrypt_at_rest_changed = true;
            }
        }
        // —— TIER1 运行时热更字段：改完 reload_config 即时生效,不进 restart_fields ——
        // （冷却/限流开关/每日上限/间隔/亲和性;由下方统一 reload_config 一并热应用）
        if let Some(v) = req.cooldown_enabled {
            if v != config.cooldown_enabled {
                config.cooldown_enabled = v;
                hot_changed = true;
            }
        }
        // `reload_config`（token_manager.rs:2163）已经在读这个字段并 store 进 AtomicBool，
        // 缺的只是「面板能把它写进 config」这一段 —— 所以补上这个分支即完成 TIER1 闭环，
        // 不需要动 token_manager。绝不 push 进 restart_fields。
        if let Some(v) = req.auto_disable_suspicious {
            if v != config.auto_disable_suspicious {
                config.auto_disable_suspicious = v;
                hot_changed = true;
            }
        }
        // —— 余额耗尽**自动**禁用开关（2026-08-14 新增，AdminService 内存态）——
        // 读取点在后台温和余额刷新循环：刷到「新鲜真值 remaining<=0」即自动禁用。
        // ⚠️ 该开关只存于本服务内存（model/config.rs 不在可改范围，无法落盘），
        // 重启回默认值 true。置 hot_changed 只为让响应如实回「已保存并立即生效」。
        if let Some(v) = req.auto_disable_quota_exceeded {
            let cur = self
                .auto_disable_quota_exceeded
                .load(std::sync::atomic::Ordering::Relaxed);
            if v != cur {
                self.auto_disable_quota_exceeded
                    .store(v, std::sync::atomic::Ordering::Relaxed);
                hot_changed = true;
            }
        }
        // —— 代理池自动健康调度开关（2026-08-14 新增，AdminService 内存态）——
        // 读取点在后台健康调度任务：每轮自检本开关，关闭时整轮跳过（任务常驻不做事，
        // 重开无需重挂）。⚠️ 只存于本服务内存（model/config.rs 不在可改范围，无法落盘），
        // 重启回默认值 true。置 hot_changed 只为让响应如实回「已保存并立即生效」。
        if let Some(v) = req.socks_auto_health {
            let cur = self
                .socks_auto_health
                .load(std::sync::atomic::Ordering::Relaxed);
            if v != cur {
                self.socks_auto_health
                    .store(v, std::sync::atomic::Ordering::Relaxed);
                hot_changed = true;
            }
        }
        if let Some(v) = req.all_cooling_fast_fail {
            if v != config.all_cooling_fast_fail {
                config.all_cooling_fast_fail = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rate_limit_enabled {
            if v != config.rate_limit_enabled {
                config.rate_limit_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rate_limit_daily_max {
            if v != config.rate_limit_daily_max {
                config.rate_limit_daily_max = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rate_limit_min_interval_ms {
            if v != config.rate_limit_min_interval_ms {
                config.rate_limit_min_interval_ms = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.affinity_enabled {
            if v != config.affinity_enabled {
                config.affinity_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.priority_in_balanced {
            if v != config.priority_in_balanced {
                config.priority_in_balanced = v;
                hot_changed = true;
            }
        }
        // ---- 智能调度(全部热更即时生效)。整百分比字段服务端 clamp,不信任前端。----
        if let Some(v) = req.credential_rpm_limit {
            // 全局每号 RPM 上界防 u32 极值污染(远超真实吞吐即无意义)。
            let v = v.min(100_000);
            if v != config.credential_rpm_limit {
                config.credential_rpm_limit = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.cooldown_scale_pct {
            let v = v.clamp(10, 500);
            if v != config.cooldown_scale_pct {
                config.cooldown_scale_pct = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rate_limit_jitter_pct {
            let v = v.min(50);
            if v != config.rate_limit_jitter_pct {
                config.rate_limit_jitter_pct = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rpm_headroom_factor {
            let v = v.min(100);
            if v != config.rpm_headroom_factor {
                config.rpm_headroom_factor = v;
                hot_changed = true;
            }
        }
        // ---- 限流档位（2026-08-11）----
        //
        // 🔴 与**文件加载**时的语义不同，这里必须真的把档位值写进配置。
        //
        // 文件加载走 `Config::apply_throttle_profile`，契约是「只填空、不覆盖显式值」——
        // 因为那时无法区分"用户想要 false"和"字段缺失默认 false"，而线上 config.json
        // 那 7 个字段全部显式写过，冲掉就是改写生产配置。
        //
        // 但从面板切档是**用户主动的意图表达**：他就是要这一档的行为。此时若还"只填空"，
        // 由于 config.json 里那些键都已存在，档位会**一个字段都改不动** —— 按钮点了没反应，
        // 这是比"冲掉配置"更糟的体验（静默无效）。
        // 所以这里用空 explicit 集合调用，让档位对所有受管字段生效，
        // 且改动会随 `save()` 落盘成显式值（之后重启加载时它们就是"显式"的，不会被再次覆盖 —— 自洽）。
        if let Some(m) = req.scheduling_mode {
            if m != config.scheduling_mode {
                config.scheduling_mode = m;
                // 调度模式映射到对应 ThrottleProfile 并写入预设矩阵
                //（smart→Direct / stable→Shielded / manual→Manual，见 `SchedulingMode`）。
                config.throttle_profile = m.to_throttle_profile();
                config.apply_throttle_profile_for_explicit_switch();
                hot_changed = true;
            }
        }
        if let Some(p) = req.throttle_profile {
            if p != config.throttle_profile {
                config.throttle_profile = p;
                // 反向同步：老客户端只发 throttleProfile 时，调度模式标记保持一致
                //（direct→smart / shielded→stable / manual→manual）。
                config.scheduling_mode = match p {
                    crate::model::config::ThrottleProfile::Direct => {
                        crate::model::config::SchedulingMode::Smart
                    }
                    crate::model::config::ThrottleProfile::Shielded => {
                        crate::model::config::SchedulingMode::Stable
                    }
                    crate::model::config::ThrottleProfile::Manual => {
                        crate::model::config::SchedulingMode::Manual
                    }
                };
                config.apply_throttle_profile_for_explicit_switch();
                hot_changed = true;
            }
        }
        // ---- 入站请求整形 + RPM 自动挡(全热更)----
        if let Some(v) = req.inbound_throttle_enabled {
            if v != config.inbound_throttle_enabled {
                config.inbound_throttle_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_rpm_auto {
            if v != config.inbound_rpm_auto {
                config.inbound_rpm_auto = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_target_rpm {
            let v = v.clamp(1, 100_000);
            if v != config.inbound_target_rpm {
                config.inbound_target_rpm = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_rpm_min {
            let v = v.clamp(1, 100_000);
            if v != config.inbound_rpm_min {
                config.inbound_rpm_min = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_rpm_max {
            let v = v.clamp(1, 100_000);
            if v != config.inbound_rpm_max {
                config.inbound_rpm_max = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_burst_secs {
            let v = v.clamp(1, 60);
            if v != config.inbound_burst_secs {
                config.inbound_burst_secs = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_queue_max_wait_secs {
            let v = v.clamp(1, 300);
            if v != config.inbound_queue_max_wait_secs {
                config.inbound_queue_max_wait_secs = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.inbound_queue_timeout_passthrough {
            if v != config.inbound_queue_timeout_passthrough {
                config.inbound_queue_timeout_passthrough = v;
                hot_changed = true;
            }
        }
        // ⭐ 三个 RPM 字段的**交叉**不变量：`min <= target <= max`。
        //
        // 必须在三者都处理完之后统一收口 —— 上面每个字段各自只 clamp 到 [1,100_000]，
        // 彼此不可见，于是能存出自相矛盾的组合。两个实测后果：
        //
        // ① **`min > max` 会 panic**：`throttle.rs` 的 `clamp(lo, hi)` 在 min>max 时
        //    panic（`u32::clamp` 的契约）。面板保存一次这样的配置就打死正在服务的进程。
        //    throttle 侧已加 `.max(lo)` 兜底，这里再拦一道，让**存下去的值**本身就自洽
        //    （否则面板显示的与实际生效的永远不一致，排查时会被带偏）。
        //
        // ② **`target > max` 让自动调节永久失效**（线上实测）：throttle 把 target
        //    clamp 到 max 后**只存在内存里**，config.json 仍留着未被 clamp 的原值。
        //    VPS 上的 `throttle-autotune` 读的是**存储值**，于是它拿一个从未生效过的
        //    数（614）跟自己的建议比 → 死区永远满足 → 永不调整，而实际生效的是 300。
        //    实测该差距在两天内从 307 扩大到 614，且仍在扩大。
        //    存储值与生效值统一后，autotune 读到的就是真值，死区判断才有意义。
        {
            let lo = config.inbound_rpm_min;
            if config.inbound_rpm_max < lo {
                tracing::warn!(
                    inbound_rpm_min = lo,
                    inbound_rpm_max = config.inbound_rpm_max,
                    "inboundRpmMax 小于 inboundRpmMin，已抬到与下限相等（否则整形层 clamp 会 panic）"
                );
                config.inbound_rpm_max = lo;
                hot_changed = true;
            }
            let clamped = config.inbound_target_rpm.clamp(lo, config.inbound_rpm_max);
            if clamped != config.inbound_target_rpm {
                tracing::warn!(
                    requested = config.inbound_target_rpm,
                    effective = clamped,
                    inbound_rpm_min = lo,
                    inbound_rpm_max = config.inbound_rpm_max,
                    "inboundTargetRpm 超出 [min,max]，已按生效值落盘（存储值与生效值必须一致，\
                     否则外部自动调节读到的是从未生效过的数）"
                );
                config.inbound_target_rpm = clamped;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rpm_reserve_slots {
            // 预留名额上界防 u32 极值污染(远超真实 RPM 容量即无意义,100_000 与 rpm_limit 上界一致)。
            let v = v.min(100_000);
            if v != config.rpm_reserve_slots {
                config.rpm_reserve_slots = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.rpm_hard_gate_overload_wait {
            if v != config.rpm_hard_gate_overload_wait {
                config.rpm_hard_gate_overload_wait = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.balance_weight_enabled {
            if v != config.balance_weight_enabled {
                config.balance_weight_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.balance_weight_floor {
            let v = v.min(100);
            if v != config.balance_weight_floor {
                config.balance_weight_floor = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.health_429_weight_enabled {
            if v != config.health_429_weight_enabled {
                config.health_429_weight_enabled = v;
                hot_changed = true;
            }
        }
        if let Some(v) = req.proxy_url {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                if config.proxy_url.is_some() {
                    config.proxy_url = None;
                    restart_fields.push("proxyUrl".into());
                }
            } else {
                // 与凭据上号同口径：URL 内嵌账密拆进独立字段，proxyUrl 只留干净 host。
                let (clean, inline_user, inline_pass) =
                    crate::http_client::split_proxy_credentials(trimmed);
                let new_val = Some(clean);
                if new_val != config.proxy_url {
                    config.proxy_url = new_val;
                    restart_fields.push("proxyUrl".into());
                }
                // 独立账密字段优先（本请求显式给了就走下面两个分支）；
                // 缺省时才回退 URL 内嵌值。URL 无 userinfo 时不把已存账密清掉。
                if req.proxy_username.is_none() {
                    if let Some(user) = inline_user {
                        if Some(&user) != config.proxy_username.as_ref() {
                            config.proxy_username = Some(user);
                            restart_fields.push("proxyUsername".into());
                        }
                    }
                }
                if req.proxy_password.is_none() {
                    if let Some(pass) = inline_pass {
                        if Some(&pass) != config.proxy_password.as_ref() {
                            config.proxy_password = Some(pass);
                            restart_fields.push("proxyPassword".into());
                        }
                    }
                }
            }
        }
        // 代理账密：前端出于安全不回显已存值,只在非空时更新;显式传空串表示清除。
        if let Some(v) = req.proxy_username {
            let new_val = if v.trim().is_empty() { None } else { Some(v.trim().to_string()) };
            if new_val != config.proxy_username {
                config.proxy_username = new_val;
                restart_fields.push("proxyUsername".into());
            }
        }
        if let Some(v) = req.proxy_password {
            let new_val = if v.is_empty() { None } else { Some(v) };
            if new_val != config.proxy_password {
                config.proxy_password = new_val;
                restart_fields.push("proxyPassword".into());
            }
        }
        if let Some(v) = req.callback_base_url {
            let trimmed = v.trim();
            let new_val = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.trim_end_matches('/').to_string())
            };
            if new_val != config.callback_base_url {
                config.callback_base_url = new_val;
                restart_fields.push("callbackBaseUrl".into());
            }
        }
        // userKey（下游对话 api_key）：仅在非空白时更新（防 fail-open：空 key 会让 /v1 匿名可达）。
        // 前端不回显现值，传空串=不改。
        // 【不再需要重启】鉴权已改为活读 `common::auth_keys` 的进程级单元，存盘后调 setter
        // 即时生效——轮换密钥是常规运维动作，重启整个网关会掐断所有在途流式请求。
        if let Some(v) = req.api_key {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                let new_val = Some(trimmed.to_string());
                if new_val != config.api_key {
                    config.api_key = new_val;
                    user_key_changed = Some(trimmed.to_string());
                }
            }
        }
        // adminApiKey：同 userKey，空串=不改（防把管理面锁死成 fail-closed 全 401）。
        // 【自锁风险】轮换后当前面板持有的旧 key 立即失效，前端须用新 key 重新鉴权——
        // 这是热更的正确语义（旧 key 必须马上作废），前端负责换 header 而非后端延迟生效。
        if let Some(v) = req.admin_api_key {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                let new_val = Some(trimmed.to_string());
                if new_val != config.admin_api_key {
                    config.admin_api_key = new_val;
                    admin_key_changed = Some(trimmed.to_string());
                }
            }
        }

        // —— 反代安全（批次3，均需重启生效）——
        if let Some(v) = req.cors_allowed_origins {
            // 去空白、去空项，保持整表替换语义
            let cleaned: Vec<String> =
                v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if cleaned != config.cors_allowed_origins {
                config.cors_allowed_origins = cleaned;
                restart_fields.push("corsAllowedOrigins".into());
            }
        }
        if let Some(v) = req.ip_allowlist {
            let cleaned: Vec<String> =
                v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            // 校验每条 CIDR 合法，非法直接拒绝（避免静默丢弃导致白名单形同虚设）
            for entry in &cleaned {
                if let Err(e) = crate::common::security::validate_cidr(entry) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "ipAllowlist 条目 '{entry}' 非法: {e}"
                    )));
                }
            }
            if cleaned != config.ip_allowlist {
                config.ip_allowlist = cleaned;
                restart_fields.push("ipAllowlist".into());
            }
        }
        if let Some(v) = req.ip_blocklist {
            let cleaned: Vec<String> =
                v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            // 校验每条 CIDR 合法,非法直接拒绝。
            for entry in &cleaned {
                if let Err(e) = crate::common::security::validate_cidr(entry) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "ipBlocklist 条目 '{entry}' 非法: {e}"
                    )));
                }
            }
            if cleaned != config.ip_blocklist {
                config.ip_blocklist = cleaned.clone();
                // 业务层黑名单镜像热更(按真实客户端 IP 封禁,反代后也生效,立即生效无需重启)。
                // 注:security 中间件的黑名单仍是 restart-only(启动时建),但业务层这道已足够拦截。
                crate::anthropic::handlers::set_ip_blocklist(&cleaned);
                hot_changed = true;
            }
        }
        if let Some(v) = req.machine_code_blocklist {
            // 归一化:trim + 小写(判定端大小写不敏感);校验格式 MC- + 12 位十六进制,非法直接拒绝。
            let cleaned: Vec<String> = v
                .into_iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            for entry in &cleaned {
                let ok = entry.len() == 15
                    && entry.starts_with("mc-")
                    && entry[3..].chars().all(|c| c.is_ascii_hexdigit());
                if !ok {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "machineCodeBlocklist 条目 '{entry}' 非法(应为 MC- 加 12 位十六进制)"
                    )));
                }
            }
            if cleaned != config.machine_code_blocklist {
                config.machine_code_blocklist = cleaned.clone();
                // 业务层机器码黑名单镜像热更(立即生效无需重启)。
                crate::anthropic::handlers::set_machine_code_blocklist(&cleaned);
                hot_changed = true;
            }
        }
        if let Some(v) = req.trust_forwarded_header {
            if v != config.trust_forwarded_header {
                config.trust_forwarded_header = v;
                restart_fields.push("trustForwardedHeader".into());
            }
        }
        if let Some(v) = req.ingress_rate_limit_per_min {
            if v != config.ingress_rate_limit_per_min {
                config.ingress_rate_limit_per_min = v;
                restart_fields.push("ingressRateLimitPerMin".into());
            }
        }
        if let Some(v) = req.max_body_bytes {
            if v != config.max_body_bytes {
                config.max_body_bytes = v;
                restart_fields.push("maxBodyBytes".into());
            }
        }

        // —— 主动 token 预刷新（批次4.4，TIER2 后台任务热更：改后 respawn 即时生效不重启）——
        if let Some(v) = req.proactive_token_refresh {
            if v != config.proactive_token_refresh {
                config.proactive_token_refresh = v;
                refresh_task_changed = true;
            }
        }
        if let Some(v) = req.token_refresh_lead_minutes {
            if v != config.token_refresh_lead_minutes {
                config.token_refresh_lead_minutes = v;
                refresh_task_changed = true;
            }
        }
        if let Some(v) = req.token_refresh_interval_secs {
            if v != config.token_refresh_interval_secs {
                config.token_refresh_interval_secs = v;
                refresh_task_changed = true;
            }
        }

        // —— 余额同步（A6，TIER2 后台任务热更：改后 respawn 即时生效不重启）——
        if let Some(v) = req.balance_refresh_interval_secs {
            if v != config.balance_refresh_interval_secs {
                config.balance_refresh_interval_secs = v;
                balance_task_changed = true;
            }
        }

        // —— 立即生效的字段：登录页背景开关 ——
        // 关闭时 random-bg 立即返回 null、后台预取轮次也会自我短路，不需重启。
        let mut login_bg_changed: Option<bool> = None;
        if let Some(v) = req.login_background_enabled {
            if v != config.login_background_enabled {
                config.login_background_enabled = v;
                login_bg_changed = Some(v);
            }
        }

        // —— 立即生效的字段：登录页背景 R18 开关 ——
        // 改后下一轮后台预取 / 池空实时兜底拉取即按新 r18 参数取图，不需重启。
        let mut login_bg_r18_changed: Option<bool> = None;
        if let Some(v) = req.login_background_r18 {
            if v != config.login_background_r18 {
                config.login_background_r18 = v;
                login_bg_r18_changed = Some(v);
            }
        }

        // —— 立即生效的字段：指纹采集开关（隐私）——
        // 关闭后热路径不再解析 device/ip/os/browser，用量记录留空；无需重启。
        let mut fingerprint_changed: Option<bool> = None;
        if let Some(v) = req.collect_client_fingerprint {
            if v != config.collect_client_fingerprint {
                config.collect_client_fingerprint = v;
                fingerprint_changed = Some(v);
            }
        }

        // —— 立即生效的字段：负载均衡模式（并入 TIER1 统一 reload 热应用）——
        if let Some(mode) = req.load_balancing_mode {
            if mode != "priority" && mode != "balanced" {
                return Err(AdminServiceError::InvalidCredential(
                    "loadBalancingMode 必须是 'priority' 或 'balanced'".to_string(),
                ));
            }
            config.load_balancing_mode = mode;
            hot_changed = true;
        }

        // —— 立即生效的字段：全局模型映射（整表替换）——
        // provider 每次调用时 `token_manager.config()`（ArcSwap load_full）取新快照，
        // 所以只需保存 + reload_config 热应用即可，无需重启（TIER1 范式，同吸收层）。
        if let Some(mm) = req.model_mapping {
            if mm != config.model_mapping {
                config.model_mapping = mm;
                hot_changed = true;
            }
        }

        // —— 立即生效的字段：错误码/提示词覆盖表（per-key merge）——
        // 消费点（错误翻译处）读 handlers 进程镜像（reload_config 改写同一镜像），
        // 所以只需保存 + reload_config 热应用。⚠️ 语义：**per-key merge**——提交的
        // key 更新为提交值（字段 None = 用内置默认），空对象 `{}` = 清掉该 key 回默认，
        // **未提交的 key 保持不变**（前端按"有改动的 key"提交，整表替换会重置用户
        // 未改的 key）。⚠️ 先校验再写盘：任一 key 非法 → 整表拒绝（保持旧表），
        // 400 回显第一个错误（对齐 exhausted_status 白名单先例）。
        if let Some(em) = req.error_messages {
            let mut merged = config.error_messages.clone();
            for (k, v) in em {
                let is_empty = v.status.is_none()
                    && v.r#type.is_none()
                    && v.message.is_none()
                    && v.retry_after_secs.is_none();
                if is_empty {
                    merged.remove(&k);
                } else {
                    merged.insert(k, v);
                }
            }
            if merged != config.error_messages {
                validate_error_messages(&merged).map_err(AdminServiceError::InvalidCredential)?;
                config.error_messages = merged;
                error_messages_changed = true;
            }
        }

        // 持久化（一次写盘）
        //
        // 2026-08-14 新增两件事：
        // ① 写盘前轮换 .bak（保留 .bak / .bak.1 / .bak.2 三份，见 rotate_config_backup），
        //    手滑改错配置可回退；
        // ② 字段级 diff 审计：对比 load 时的旧值与改完的新值，只记字段名不记值
        //    （敏感字段的值绝不进日志）。
        rotate_config_backup(&config_path);
        {
            let new_json = serde_json::to_value(&config).unwrap_or_default();
            let changed = diff_json_fields(&old_json, &new_json);
            if !changed.is_empty() {
                tracing::info!(target: "audit", "配置更新，变更字段: {:?}", changed);
            }
        }
        config
            .save()
            .map_err(|e| AdminServiceError::InternalError(format!("保存配置失败: {}", e)))?;

        // 配置快照(get_config_snapshot)读的是 token_manager.config()(ArcSwap 内存 config)。
        // 只要有**运行时/展示类**字段落盘,就 reload_config 把 ArcSwap 与磁盘对齐,否则快照会读到旧值——
        // ⭐这正是"关掉 R18/背景图保存后、刷新页面开关又变回开"的根因:那些字段过去只更运行时镜像
        //   (AtomicBool)+存盘,却没 reload ArcSwap,导致快照永远回读 ArcSwap 里的旧值。
        // reload_config 从盘重读整份 config 原子换入 ArcSwap(含 login_background/fingerprint/
        // extract_thinking 等所有热字段),幂等安全。
        //
        // ⚠️【proxy split-brain 修复】**绝不因 restart-only 字段(proxyUrl/tls/host/port/callback/
        // adminKey 等)触发 reload**。这些固化项在启动时已被固化到运行态(如 KiroProvider.self.proxy
        // 由 new() 一次性赋值,对话/刷新路径全程用它),而登录流(social/idc/external_idp)却是
        // **活读 config().proxy_url**。若改了 proxyUrl 就 reload 换进 ArcSwap:登录流立刻走新代理、
        // 对话/刷新流仍走启动固化的旧代理 = split-brain(功能性割裂,与"改这些需重启"的语义矛盾)。
        // 故这类字段只进 restart_fields 提示前端重启,ArcSwap 保持旧值 → 全局一致(全旧,重启才全新)。
        // 展示/热字段各有独立 *_changed 标志,不依赖 restart_fields,R18 stale 根治不受影响。
        let hot_or_display_changed = hot_changed
            || refresh_task_changed
            || balance_task_changed
            || login_bg_changed.is_some()
            || login_bg_r18_changed.is_some()
            || fingerprint_changed.is_some()
            || extract_thinking_changed.is_some()
            || cc_auto_buffer_changed.is_some()
            || import_keys_enabled_changed
            // 分身默认启用同样没有 TIER3 setter，**只**靠这一行触发 reload_config。
            // 删掉它 → 面板改了、存了盘，但 clone_default_enabled() 读到的仍是旧值。
            || clone_default_enabled_changed
            || prompt_cache_enabled_changed.is_some()
            // 透传模拟缓存有 TIER3 setter（handlers 镜像），但要 `hot_changed` 之外仍进
            // OR 链才会调它：漏掉这行只改本项时面板会回「无改动」、镜像不刷新。
            || mock_cache_changed
            || strip_env_noise_changed.is_some()
            // Kiro 原生 effort 开关有 TIER3 setter（converter 镜像），但要 `hot_changed`
            // 之外仍进 OR 链才会调它：漏掉这行只改本项时面板会回「无改动」。
            || native_thinking_effort_enabled_changed.is_some()
            // CC↔Kiro 工具名/参数映射开关同款：TIER3 setter（converter 镜像），漏掉这行
            // 只改本项时面板会回「无改动」、镜像不刷新。
            || tool_compat_mapping_changed.is_some()
            || self_heal_changed
            || tool_clean_leaked_tokens_changed.is_some()
            || tool_stream_align_failure_changed.is_some()
            || tool_expose_error_to_client_changed.is_some()
            || tool_repair_json_changed.is_some()
            || tool_truncation_recovery_changed.is_some()
            || tool_description_max_chars_changed.is_some()
            // 🔴 吸收层没有 TIER3 setter，**只**靠这一行触发 reload_config 把新值换进 ArcSwap。
            // 删掉它 → 面板改了、存了盘、但 provider 读到的仍是旧值 → 开关静默无效。
            // 由 absorb_changed_is_in_hot_reload_or_chain 源码守卫钉死。
            || absorb_changed
            // 错误码/提示词覆盖表同款：消费点每请求读 config ArcSwap（无 TIER3 setter），
            // 只有这一行能触发 reload_config。漏掉 → 存盘但热路径仍读旧表。
            || error_messages_changed;
        if hot_or_display_changed {
            if let Err(e) = self.token_manager.reload_config() {
                tracing::warn!("配置已存盘但热重载失败,下次重启生效: {}", e);
            }
        }

        // at-rest 加密开关变更:reload_config 后 config 已是新值,立即重写凭据+回收站文件(明文↔密文),
        // 让开/关即时落到磁盘,而非等下次偶发凭据变更。失败仅告警(下次 persist 会补上)。
        if encrypt_at_rest_changed {
            match self.token_manager.repersist_secrets() {
                Ok(true) => tracing::info!("at-rest 加密开关已改,已立即重写凭据/回收站文件"),
                Ok(false) => tracing::warn!(
                    "at-rest 加密开关已改,但立即重写凭据文件被跳过（无凭据路径）"
                ),
                Err(e) => tracing::warn!("at-rest 加密开关已改,但立即重写凭据文件失败(下次变更会补上): {}", e),
            }
        }

        // TIER2 后台任务热重挂（读已 reload 的最新 config，abort 旧任务 + 按需 respawn）。
        if refresh_task_changed {
            self.token_manager.respawn_refresh_task();
        }
        if balance_task_changed {
            self.respawn_balance_task();
        }

        // 登录页背景开关立即应用到运行时镜像（下一次 random-bg / 预取轮次即生效）
        if let Some(v) = login_bg_changed {
            crate::admin_ui::set_login_background_enabled(v);
        }

        // 登录页背景 R18 开关立即应用到运行时镜像（下一轮预取 / 池空兜底拉取即按新参数）
        if let Some(v) = login_bg_r18_changed {
            crate::admin_ui::set_login_background_r18(v);
        }

        // ⭐修复"关闭 R18/背景后缓存不清、刷新还是旧图":开关一变就**立即清空背景图内存池**。
        // 否则池里已缓存的旧参数图(R18/全年龄)会一直服务到自然淘汰完(容量20、每12分钟才补6张),
        // 表现为"关了 R18 保存后刷新仍是旧图"。清池后下次 random-bg 按新参数即时重新拉取。
        if login_bg_r18_changed.is_some() || login_bg_changed.is_some() {
            let cleared = crate::admin_ui::clear_bg_pool();
            tracing::info!("登录背景开关变更,已清空背景图缓存池({} 张)", cleared);
            // ⭐清池后若背景图当前为开启态,立即补一批新参数图填池(不等常驻循环的下一轮 12min tick)。
            // 否则:开启背景图/切换 R18 后池是空的,登录页只能走单张实时兜底(慢/偶尔失败),
            // 表现为"第一次没图、关开偶尔显示一次、再刷新又没"——本次连同预取循环常驻一起根治。
            if config.login_background_enabled {
                crate::admin_ui::trigger_bg_refill();
                tracing::info!("背景图已开启,已触发即时补池(按新参数预取一批)");
            }
        }

        // 指纹采集开关立即应用到热路径运行时镜像（下一个请求即生效）
        if let Some(v) = fingerprint_changed {
            crate::anthropic::set_collect_client_fingerprint(v);
        }

        // TIER3：thinking 提取开关立即应用到热路径进程级镜像（下一个非流式请求即生效）
        if let Some(v) = extract_thinking_changed {
            crate::anthropic::set_extract_thinking(v);
        }

        // TIER3：CC 自动切缓冲开关立即应用到热路径进程级镜像（下一个流式请求即生效）
        if let Some(v) = cc_auto_buffer_changed {
            crate::anthropic::set_cc_auto_buffer(v);
        }

        // TIER3：prompt cache 记账下发开关立即应用到热路径进程级镜像（下一个请求即生效）
        if let Some(v) = prompt_cache_enabled_changed {
            crate::anthropic::set_prompt_cache_enabled(v);
        }

        // TIER3：透传模拟缓存配置立即应用到热路径进程级镜像（下一个透传请求即生效）。
        // 用 `config`（已更新）而非 req 原值：两个字段可能只改一个，setter 要拿完整组。
        if mock_cache_changed {
            crate::anthropic::handlers::set_mock_cache_config(
                config.mock_cache_enabled,
                config.mock_cache_read_ratio,
            );
        }

        // 环境噪音剥离开关立即应用到 converter 进程级镜像（下一个请求即生效）
        if let Some(v) = strip_env_noise_changed {
            crate::anthropic::set_strip_env_noise(v);
        }
        // Kiro 原生 effort 开关立即应用到 converter 进程级镜像（下一个请求即生效）
        if let Some(v) = native_thinking_effort_enabled_changed {
            crate::anthropic::set_native_thinking_effort_enabled(v);
        }
        // CC↔Kiro 工具名/参数映射开关立即应用到 converter 进程级镜像（下一个请求即生效，不重启）。
        if let Some(v) = tool_compat_mapping_changed {
            crate::anthropic::set_tool_compat_mapping(v);
        }
        // 工具错误缓解三开关立即应用到 handlers 进程级镜像（下一个请求即生效，不重启）。
        if let Some(v) = tool_clean_leaked_tokens_changed {
            crate::anthropic::set_tool_clean_leaked_tokens(v);
        }
        if let Some(v) = tool_stream_align_failure_changed {
            crate::anthropic::set_tool_stream_align_failure(v);
        }
        if let Some(v) = tool_expose_error_to_client_changed {
            crate::anthropic::set_tool_expose_error_to_client(v);
        }
        if let Some(v) = tool_repair_json_changed {
            crate::anthropic::set_tool_repair_json(v);
        }
        if let Some(v) = tool_truncation_recovery_changed {
            crate::anthropic::set_tool_truncation_recovery(v);
        }
        // 工具描述上限立即应用到 converter 进程级镜像（下一个请求即生效，不重启）。
        if let Some(v) = tool_description_max_chars_changed {
            crate::anthropic::set_tool_description_max_chars(v);
        }

        // userKey 轮换立即生效：下一个 /v1 请求即按新 key 判定，旧 key 同时失效。
        // ⚠️必须放在 reload_config 之后——reload 会把 config 里的 userKey 钉回启动值
        // （restart-only 字段的 split-brain 防护，见 token_manager::reload_config 的
        // restore 表），但热更单元才是鉴权的活真相源，故此处后写、以新值为准。
        // setter 拒空，失败仅告警（旧 key 继续有效，不会裸奔）。
        if let Some(v) = &user_key_changed {
            match crate::common::auth_keys::set_user_key(v) {
                Ok(()) => tracing::info!("apiKey 已轮换并即时生效（无需重启）"),
                Err(e) => tracing::error!("apiKey 已存盘但热更失败，重启后生效: {}", e),
            }
        }
        // adminApiKey 轮换：同上。旧 key 立即失效，面板须用新 key 重新鉴权。
        if let Some(v) = &admin_key_changed {
            match crate::common::auth_keys::set_admin_key(v) {
                Ok(()) => tracing::info!("adminApiKey 已轮换并即时生效（无需重启）"),
                Err(e) => tracing::error!("adminApiKey 已存盘但热更失败，重启后生效: {}", e),
            }
        }

        let immediate_changed = hot_changed
            || refresh_task_changed
            || balance_task_changed
            || login_bg_changed.is_some()
            || login_bg_r18_changed.is_some()
            || fingerprint_changed.is_some()
            || extract_thinking_changed.is_some()
            || cc_auto_buffer_changed.is_some()
            || import_keys_enabled_changed
            // 立即生效项（reload_config 换 ArcSwap），漏掉这行只改本项时面板会回
            // 「无改动」，与实际不符。
            || clone_default_enabled_changed
            || prompt_cache_enabled_changed.is_some()
            || mock_cache_changed
            || strip_env_noise_changed.is_some()
            || native_thinking_effort_enabled_changed.is_some()
            || tool_compat_mapping_changed.is_some()
            || tool_clean_leaked_tokens_changed.is_some()
            || tool_stream_align_failure_changed.is_some()
            || tool_expose_error_to_client_changed.is_some()
            || tool_repair_json_changed.is_some()
            || tool_truncation_recovery_changed.is_some()
            || tool_description_max_chars_changed.is_some()
            // 吸收层是立即生效项（reload_config 换 ArcSwap），漏掉这行只改吸收层时面板会
            // 回「未检测到变更」，与实际不符。
            || absorb_changed
            // 错误码/提示词覆盖表同款（hot_or_display_changed 触发 reload_config 即生效）：
            // 漏掉这行只改错误码表时面板会回「未检测到变更」，与实际不符。
            || error_messages_changed
            // 两把 key 走 auth_keys setter 即时生效，故算「立即生效」而非「需重启」。
            // 不进 hot_or_display_changed：reload_config 会把它们钉回启动值，重载对它们无用。
            || user_key_changed.is_some()
            || admin_key_changed.is_some();
        let restart_required = !restart_fields.is_empty();
        let message = if restart_required {
            format!("已保存。{} 个字段需重启服务后生效。", restart_fields.len())
        } else if immediate_changed {
            "已保存并立即生效（无需重启）。".to_string()
        } else {
            "无改动。".to_string()
        };

        tracing::info!(
            "配置已更新（需重启字段: {:?}, TIER1热更: {}, TIER2重挂: 预刷新={} 余额={}, TIER3: thinking={:?} envNoise={:?}）",
            restart_fields,
            hot_changed,
            refresh_task_changed,
            balance_task_changed,
            extract_thinking_changed,
            strip_env_noise_changed
        );

        Ok(UpdateConfigResponse {
            success: true,
            message,
            restart_required,
            restart_fields,
        })
    }
}
