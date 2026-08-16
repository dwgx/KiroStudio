# 调度配置旋钮全面调研 + 简化方案（docs/scheduling-config-simplify.md）

> 2026-08-16 调研落盘。目标：把「30+ 个调度旋钮」收敛成「几个按钮 + 后台逻辑智能」。
> 全部结论带源码行号（工作树当前状态）。只研究不改代码。
> 前置阅读：src/model/config.rs（全文）、src/model/error_messages.rs（throttleProfile 相关，
> 实际在其中无字段，错误码表独立）、docs/blockers-config.md（面板摸不到字段 + restore 表）。

---

## 0. 一句话结论

- **调度相关配置字段共 36 个**（不含安全组）：冷却 7 + 入站整形 8 + 全局/选号 RPM 4 +
  拟人限流 4 + 吸收层 10 + 选号 3（另加凭据级 3 个）。按真实价值分级：
  **A 级 5 个**（用户真需要调）、**B 级 9 个**（高级用户）、**C 级 15 个**（默认值即对）、
  **D 级 7 个**（死旋钮/从未触发）。
- **线上部署形态（4 凭据全走 custom_api 透传）下，吸收层 10 个旋钮全部无效**
  —— 吸收层只挂 Kiro 主路径（`provider.rs` 的 `absorb:` 循环），不透传路径。
  面板已有 `absorbHasNoEffect` 警告（settings-page.tsx:1854），但这 10 个旋钮
  依然占着 UI 一整张卡。
- **简化方案**：面板调度分区顶部放 3 个按钮 —— 「智能调度（推荐）/ 稳定优先 / 高级手动」，
  复用现有 `ThrottleProfile` 机制（Direct/Shielded/Manual）并**扩展其覆盖矩阵**，
  全部细项旋钮收进「高级参数」折叠区（现状齿轮卡已基本完成，只需补收剩件）。
  被隐藏的 C/D 级旋钮全部保留解析与消费，只是默认值即正确、无需用户碰。

---

## 1. 调度配置旋钮全量清单（分组 + 分级）

### 1.1 冷却类（失败反应，7 个）

| # | 字段 | 类型 | 默认 | 消费点 | 面板 | 分级 |
|---|---|---|---|---|---|---|
| C1 | `cooldown_enabled` | bool | true | token_manager.rs:3435/3562/6189（Atomic 镜像） | ✓ 主开关 | **A** |
| C2 | `cooldown_scale_pct` | u32 | 100 | cooldown.rs:324 `set_cooldown_scale_pct`（缩放 10..500） | ✓ 齿轮卡 | C |
| C3 | `all_cooling_fast_fail` | bool | true | token_manager.rs:4960（全池冷却快速失败） | ✓ | C |
| C4 | `auto_disable_suspicious` | bool | true | token_manager.rs:2846（风控自动禁用） | ✓ 齿轮卡 | B |
| C5 | `self_heal_base_backoff_secs` | u64 | 60 | token_manager.rs:4796（自愈指数退避基值） | ✓ 高级卡 | B |
| C6 | `self_heal_max_backoff_secs` | u64 | 900 | token_manager.rs:4796（退避上限） | ✓ 高级卡 | C |
| C7 | `self_heal_max_shift` | u32 | 4 | token_manager.rs（2^n 指数 clamp） | ✓ 高级卡 | D |

**冷却变体时长表**（cooldown.rs:107-129）：9 种 `CooldownReason` 时长硬编码
（RateLimit 15s / Suspicious 20s / AuthTransient 20s / ServerError 30s /
TokenRefresh 60s / ModelUnavailable 300s / 认证失败+账户暂停+配额耗尽 86400s），
**没有 per-reason 旋钮**，只有全局 scale。其中后 3 个 86400s 长冷却硬窗
「永不自动恢复」—— 这是用户口中的**3 个死冷却变体**：无配置对应、无 UI、
只能靠人工/自愈干预，scale 明确不碰它们（config.rs:499「只缩放短时/瞬时冷却基数，
不动认证失败/封号那类长冷却硬窗」）。

### 1.2 入站整形类（8 个）

| # | 字段 | 类型 | 默认 | 消费点 | 面板 | 分级 |
|---|---|---|---|---|---|---|
| I1 | `inbound_throttle_enabled` | bool | true | token_manager.rs:2618（整形总开关） | ✓ | C |
| I2 | `inbound_rpm_auto` | bool | true | token_manager.rs:2619（AIMD 自动挡） | ✓ | **B** |
| I3 | `inbound_target_rpm` | u32 | 100 | token_manager.rs:2620（目标 RPM） | ✓ | C |
| I4 | `inbound_rpm_min` | u32 | 20 | token_manager.rs:2621（AIMD 下限） | ✓ | C |
| I5 | `inbound_rpm_max` | u32 | 300 | token_manager.rs:2621（AIMD 上限） | ✓ | C |
| I6 | `inbound_burst_secs` | u32 | 2 | token_manager.rs（突发容量） | ✓ | C |
| I7 | `inbound_queue_max_wait_secs` | u32 | 30 | token_manager.rs（排队上限） | ✓ | C |
| I8 | `inbound_queue_timeout_passthrough` | bool | true | token_manager.rs（超时放行 vs 429） | ✓ | **B** |

⚠️ I2/I8 是**语义陷阱**（config.rs:36-40 自述）：I8 名字像「别拒绝」，实际决定
「整形层是真限流器还是延迟器」；I2 内置 AIMD 是单向棘轮（429 乘性减、回升要
20s 静默 ×N），线上实测每 6.4s 一次 429 会锁死在下限。**这两个不该裸着给用户**，
应并入档位（Shielded/Direct 已在管 I8，扩展后连 I2 一起管）。

### 1.3 全局/选号 RPM 类（4 个）

| # | 字段 | 类型 | 默认 | 消费点 | 面板 | 分级 |
|---|---|---|---|---|---|---|
| R1 | `credential_rpm_limit` | u32 | 0（→兜底 30） | token_manager.rs:4444 `effective_saturation_limit` | ✓ 齿轮卡 | **B** |
| R2 | `rpm_headroom_factor` | u32 | 85 | token_manager.rs:4458 `apply_rpm_headroom` | ✓ 主卡+齿轮卡（**双份 UI**） | C |
| R3 | `rpm_reserve_slots` | u32 | 0 | token_manager.rs:4466 | ✓ 齿轮卡 | D |
| R4 | `rpm_hard_gate_overload_wait` | bool | false | token_manager.rs（整池饱和背压等待） | ✓ 齿轮卡 | C |

⚠️ R2 在主卡和齿轮卡各有一份 UI（settings-page.tsx:2450 与 :2483），重复。
R3 默认 0 = 与「无此功能」等价，语义模糊，建议隐藏。

### 1.4 拟人限流类（4 个）

| # | 字段 | 类型 | 默认 | 消费点 | 面板 | 分级 |
|---|---|---|---|---|---|---|
| L1 | `rate_limit_enabled` | bool | false | token_manager.rs:4259/4325/5248/5886/6109/6159 | ✓ 主开关 | **B** |
| L2 | `rate_limit_daily_max` | u32 | 500 | token_manager.rs:2638 | ✓ 齿轮卡+主卡（**双份 UI**） | C |
| L3 | `rate_limit_min_interval_ms` | u64 | 1000 | token_manager.rs:2639 | ✓ 齿轮卡+主卡（**双份 UI**） | C |
| L4 | `rate_limit_jitter_pct` | u32 | 20 | token_manager.rs:2641 | ✓ 齿轮卡 | C |

⚠️ L2/L3 同样双份 UI（settings-page.tsx:3037/3062 与 :3040/3065）。L1 默认关有
实测理由（间隔 1s/请求拖慢高频工具调用），是「要开才开」的开关。

### 1.5 吸收层类（10 个）

| # | 字段 | 类型 | 默认 | 消费点 | 面板 | 分级 |
|---|---|---|---|---|---|---|
| A1 | `upstream_retry_absorb_enabled` | bool | false | provider.rs:282（AbsorbPolicy::from_config） | ✓ 总开关 | **B** |
| A2 | `upstream_retry_absorb_budget_secs` | u64 | 45 | provider.rs:293（≥45 下限，provider.rs:283） | ✓ | **B** |
| A3 | `upstream_retry_absorb_max_rounds` | u32 | 3 | provider.rs:249 | ✓ | **B** |
| A4 | `upstream_retry_absorb_min_delay_ms` | u64 | 150 | provider.rs:278（抬到 50ms 下限） | ✓ | C |
| A5 | `upstream_retry_absorb_max_delay_secs` | u64 | 15 | provider.rs:280 | ✓ | C |
| A6 | `upstream_retry_absorb_suspended` | bool | false | provider.rs:298（403 风控吸收） | ✓ | D |
| A7 | `upstream_retry_absorb_server_error` | bool | false | provider.rs:299（5xx 吸收） | ✓ | D |
| A8 | `upstream_retry_absorb_capacity_400` | bool | false | provider.rs:300（容量 400 吸收） | ✓ | D |
| A9 | `upstream_retry_absorb_swap_budget_secs` | u64 | 0 | provider.rs:241（换号空窗长预算） | ✓ | C |
| A10 | `upstream_retry_absorb_exhausted_status` | u16 | 503 | provider.rs:305（耗尽终态码） | ✓（503/429 切换） | **B** |

**关键事实**：① 吸收层**不覆盖透传路径**（provider.rs:1825-1826 自述）—— 线上
100% 流量走透传 ⇒ 这 10 个旋钮在线上**全程无效**（面板已有 noEffect 警告）。
② A6/A7/A8 默认关各有实测依据：A7 外挂 11.6 次重试才救回 1 个请求（config.rs:342-344）；
A6 与自愈退避冲突（config.rs:327-333）；A8 误认真限流（config.rs:359-363）。
③ A10 是产品级差异：Cursor 见 429 掐会话、见 503 自动退避（config.rs:397-404）。

### 1.6 选号类（3 个 + 凭据级 3 个）

| # | 字段 | 类型 | 默认 | 消费点 | 面板 | 分级 |
|---|---|---|---|---|---|---|
| S1 | `load_balancing_mode` | String | "priority" | token_manager.rs:4558 `effective_scheduling` | ✓ 按钮（已有） | **A** |
| S2 | `priority_in_balanced` | bool | false | token_manager.rs:4562（balanced 下按优先级分层） | ✓ | **A** |
| S3 | `affinity_enabled` | bool | true | token_manager.rs:3646/4079（会话亲和） | ✓ | **B** |
| S4 | `balance_weight_enabled` | bool | true | token_manager.rs:2691（余额加权） | ✓ | C |
| S5 | `balance_weight_floor` | u32 | 50 | token_manager.rs:2692 | ✓ | C |
| S6 | `health_429_weight_enabled` | bool | true | token_manager.rs:2679（429 EWMA 降权） | ✓ | C |
| S7 | `custom_api_first`（全局） | bool | false | 透传选号（凭据级可覆盖） | 凭据级有 UI | **B** |
| S8 | `overload_fallback_model` | Option | None | provider（容量耗尽回退模型） | 无 UI | **B** |

凭据级（不在 Config，属选号门）：`allowed_models`（模型白名单，选号门判据）、
`rpm_limit`（per-cred RPM，`effective_saturation_limit` 最高优先）、
`priority`（选号权重）—— 这三个是**真正该留给用户调的 A 级旋钮**，且已有 UI。

### 1.7 并发闸类（2 个，面板摸不到）

| # | 字段 | 类型 | 默认 | 消费点 | 面板 | 分级 |
|---|---|---|---|---|---|---|
| G1 | `upstream_concurrency_limit` | usize | 16 | provider.rs:1028（构造时固化，**重启生效**） | 无 UI（快照也没有） | C |
| G2 | `upstream_per_credential_limit` | usize | 8 | provider.rs:1038（同上） | 无 UI | C |

### 1.8 安全组（非调度，仅归类，5 个，全 A 级必须保留）

`cors_allowed_origins` / `ip_allowlist` / `ip_blocklist` / `machine_code_blocklist` /
`ingress_rate_limit_per_min` —— 反代安全，与调度无关，**不并入本次简化**（但注意
blockers-config.md §3.2 的 restore 表缺 5 项问题，动配置接口时一并修）。

---

## 2. 旋钮价值分级汇总

| 级 | 数量 | 定义 | 成员 |
|---|---|---|---|
| **A** | 5 + 凭据级 3 | 用户真需要调，语义直观 | `load_balancing_mode`、`priority_in_balanced`、`cooldown_enabled`、`custom_api_first`、`overload_fallback_model`；凭据级 `allowed_models`/`rpm_limit`/`priority` |
| **B** | 9 | 高级用户才需要，有真实调参场景 | `auto_disable_suspicious`、`self_heal_base_backoff_secs`、`inbound_rpm_auto`、`inbound_queue_timeout_passthrough`、`credential_rpm_limit`、`rate_limit_enabled`、吸收层 A1/A2/A3/A10、`affinity_enabled` |
| **C** | 15 | 默认值就是对的，无数据支撑调参 | `cooldown_scale_pct`、`all_cooling_fast_fail`、`self_heal_max_backoff_secs`、入站整形 I1/I3-I7、`rpm_headroom_factor`、`rpm_hard_gate_overload_wait`、拟人 L2-L4、`balance_weight_enabled/floor`、`health_429_weight_enabled`、吸收层 A4/A5/A9、G1/G2 |
| **D** | 7 | 死旋钮/从未触发 | `self_heal_max_shift`、`rpm_reserve_slots`、吸收层 A6/A7/A8、`prompt_cache_ttl_seconds`（无读取点，blockers 1.3 实锤）、3 个 86400s 长冷却硬窗（无旋钮，硬编码） |

---

## 3. 简化方案（按钮/档位设计 + 默认值矩阵 + 被隐藏清单）

### 3.1 设计原则

1. **复用现有 ThrottleProfile 机制**，不新造概念：Direct/Shielded/Manual 已实现
   「只填空不覆盖」与「面板显式切换全写」两条路径（config.rs:1697-1799），
   线上 throttleProfile=direct 已在用。简化 = 扩展 Direct/Shielded 的**覆盖矩阵**
   （现在只管 4 个字段），前端把档位包装成用户按钮。
2. **默认 Manual 不变** —— 这是向前兼容硬保证（守卫
   `throttle_profile_defaults_to_manual_and_changes_nothing` 钉死），老配置零变化。
3. **隐藏 ≠ 删除**：所有被隐藏旋钮保留解析、快照、消费点。只是 UI 折叠进
   「高级参数」，默认值即正确。面板切档走显式覆盖路径，矩阵里的值会真正写入。
4. **每个被隐藏旋钮必须给「默认值就是对的」论证**（见 §5），不许凭感觉藏。

### 3.2 三按钮设计

调度分区顶部放 3 个单选按钮（替换现在的 throttleProfile 下拉）：

| 按钮 | 映射档位 | 定位 | 适用 |
|---|---|---|---|
| **智能调度（推荐，默认选中态）** | `Direct` 扩展矩阵 | 全自动：吸收/冷却/整形/RPM/选号加权全开，默认参数全家桶 | 网关直连客户端（无外挂 shield）—— 覆盖线上现状 |
| **稳定优先** | `Shielded` 扩展矩阵 | 保守：整形真限流（超时返 429）、吸收层关（防与外挂叠乘）、冷却 scale 提高 | 网关前有重试外挂（Caddy→kiro_shield.py→网关链路）；号少怕封 |
| **高级手动** | `Manual` | 一个字段都不覆盖，下方「高级参数」全展开 | 排障/专家调参 |

三档之外：**「高级参数」折叠区**（现状齿轮卡收编）承载全部 B/C 级细项，
默认折叠，展开后可逐项覆盖 —— 覆盖后的值在切档时受「显式优先」保护
（切档只填空、不覆盖显式值，config.rs:1750-1756）。

### 3.3 默认参数矩阵（智能调度 = Direct 扩展档的完整取值）

| 分组 | 字段 | 智能调度（Direct） | 稳定优先（Shielded） | 依据 |
|---|---|---|---|---|
| 吸收层 | `upstream_retry_absorb_enabled` | **true** | false | Direct 开（网关内多承担）；Shielded 关（防叠乘，config.rs:1772-1778） |
| | `..._budget_secs` | 45 | 45 | = `MAX_REQUEST_RETRY_BUDGET_SECS`（provider.rs:147），同源 min 才有意义 |
| | `..._max_rounds` | 3 | 3 | 放大上限 3×12=36，单号池最坏 4 次 |
| | `..._min_delay_ms` | 150 | 150 | 号池亚秒级恢复不该睡满 1s |
| | `..._max_delay_secs` | 15 | 15 | 与外挂 clamp 上界一致 |
| | `..._suspended/server_error/capacity_400` | false | false | 三档统一关：实测依据见 §1.5（11.6:1 吸收比/自愈冲突/误认限流） |
| | `..._swap_budget_secs` | 0 | 0 | 非零 = 单请求可占连接数分钟，部署侧决定 |
| | `..._exhausted_status` | 503 | 503 | Cursor 见 429 掐会话、503 自动退避（config.rs:1263-1268） |
| 冷却 | `cooldown_enabled` | **true** | true | 429 后坏号真退避；**线上 false 是语义陷阱**（坏号不退避=原地打转） |
| | `cooldown_scale_pct` | 100 | 150 | 智能=原时长；稳定=更保守（号少慎防封号） |
| | `all_cooling_fast_fail` | true | true | 全池冷却立即 429 让客户端退避，比网关硬扛温和 |
| | `auto_disable_suspicious` | true | true | 持续风控自动禁用，防加重封禁 |
| | `self_heal_*` | 60/900/4 | 60/900/4 | 60s 起翻倍上限 15 分钟，403 突发窗口约 10 分钟同量级 |
| 整形 | `inbound_throttle_enabled` | true | true | 削峰保护号不被打爆 |
| | `inbound_queue_timeout_passthrough` | **true** | **false** | Direct=宁可慢不要拒（不流通根治）；Shielded=真限流返 429 让外挂走 cool 分支 |
| | `inbound_rpm_auto` | **true** | true | AIMD 自动挡（线上 true） |
| | `inbound_target_rpm/min/max` | 100/20/300 | 100/20/300 | 出厂默认，实测无调参依据 |
| | `inbound_burst_secs` | 2 | 2 | 允许短时小突发不排队 |
| | `inbound_queue_max_wait_secs` | 30 | 30 | 排队最长等待 |
| RPM | `credential_rpm_limit` | 0（兜底 30） | 0（兜底 30） | 全局默认，per-cred 覆盖（凭据级 UI 才是真旋钮） |
| | `rpm_headroom_factor` | 85 | 85 | 预留 15% 缓冲，让饱和判定早于上游硬限 |
| | `rpm_reserve_slots` | 0 | 0 | 无固定突发预算需求 |
| | `rpm_hard_gate_overload_wait` | false | false | 整池饱和回退软门不阻塞 |
| 选号 | `balance_weight_enabled` | true | true | dwgx 真机观察拉平 |
| | `balance_weight_floor` | 50 | 50 | 因子 [0.5,1.0]，差 10~20% 微调 |
| | `health_429_weight_enabled` | true | true | 429 EWMA 降权是既有 health 机制 |
| | `affinity_enabled` | true | true | 会话粘账号防关联 |
| | `priority_in_balanced` | 不覆盖 | 不覆盖 | 与模式耦合，留给用户 |
| 拟人限流 | `rate_limit_*` | **全部不覆盖** | 全部不覆盖 | L1 默认关有实测理由；这是「要开才开」的组，不该被档位悄悄打开 |

> 说明：三档矩阵只覆盖「有默认答案」的字段；`load_balancing_mode`、`priority_in_balanced`、
> `rate_limit_enabled`、`custom_api_first`、`overload_fallback_model` 与档位正交，一律不覆盖。

### 3.4 被隐藏清单（UI 折叠进「高级参数」）

**完全隐藏（移出主 UI，折叠区也不放）** —— 7 个 D 级：
`self_heal_max_shift`、`rpm_reserve_slots`、`upstream_retry_absorb_suspended`、
`upstream_retry_absorb_server_error`、`upstream_retry_absorb_capacity_400`、
`prompt_cache_ttl_seconds`（本来就是 ReadonlyRow，settings-page.tsx:2877）。

**折叠进高级参数（默认折叠）** —— 15 个 C 级 + 9 个 B 级：
冷却齿轮卡（C2 已在内）、自愈三项（C5/C6/C7 已在内）、入站整形卡（I1-I8 已在内）、
RPM 齿轮卡（R1-R4 已在内，去掉主卡 R2 重复份）、拟人限流齿轮卡（L1-L4 已在内，
去掉主卡 L2/L3 重复份）、吸收层卡（A1-A10 已在内）、余额加权/429 降权（S4-S6）。

**现状盘点**：前端已有 `SettingGearCard` 齿轮卡 + `AdvancedDisclosure` 折叠壳
（settings-page.tsx:2463/2520/2957/3029、:2679），大部分收编已完成。
本次要动的实际只是：① 主卡上的 R2/L2/L3 三处重复 UI 移除；② throttleProfile
下拉 → 三按钮；③ 主卡上散落的 `all_cooling_fast_fail`、`affinity_enabled`、
`priority_in_balanced` 归位。

---

## 4. 实施步骤

### 4.1 后端（Rust）

1. **扩展 `apply_throttle_profile` 矩阵**（config.rs:1736-1799）：Direct/Shielded
   从 4 字段扩展到 §3.3 矩阵（约 20 字段）。保留 `fill!` 宏的「只填空不覆盖」契约。
   - ⚠️ `inbound_rpm_auto` 在矩阵中**置 true**：线上已 true，且智能档的定位就是
     「后台逻辑智能」；单向棘轮问题记录在注释里（config.rs:1869-1882 的语义陷阱
     守卫需同步更新——它断言默认 true，不受影响，但注释里「线上刻意 false」已过期，
     blockers-config.md §2 实锤线上已是 true，顺手修正注释）。
2. **新增测试**：`smart_profile_matrix_matches_§3_3`（钉 Direct 档 20 字段取值）、
   `shielded_profile_matrix`（钉 Shielded 档）、保留既有
   `throttle_profile_defaults_to_manual_and_changes_nothing`（Manual 零变化守卫）。
3. **`apply_throttle_profile_for_explicit_switch` 已存在**（config.rs:1725），面板切档
   路径无需改；切档后值随 `save()` 落盘成显式键，下次启动不再被加载路径覆盖（自洽）。
4. 顺手修 blockers-config.md §3.2 的 restore 表缺 5 项（token_manager.rs:2697-2729
   补 `cors_allowed_origins/ip_allowlist/trust_forwarded_header/ingress_rate_limit_per_min/max_body_bytes`），
   否则混改触发 reload 时快照说谎 —— 与本方案同批提交的配置改动越多越值得修。
5. 语义陷阱守卫注释修正（config.rs:1869-1873 的「线上刻意 false」→ 线上 true）。

### 4.2 前端（admin-ui，pnpm）

1. **三按钮**：`settingspage.throttleProfile` 下拉 → 三按钮组（复用
   `loadBalancingMode` 的按钮样式，settings-page.tsx:2375-2390）。按钮名：
   「智能调度（推荐）/ 稳定优先 / 高级手动」；选中「高级手动」时展开全部细项。
   三语 i18n 键补齐（`settingspage.throttleProfile.smart/stable/manual` + hint）。
2. **去重**：主卡上删除 R2（headroom，:2450 主卡份）、L2/L3（dailyMax/minInterval
   :3062-3067 主卡份）—— 齿轮卡内保留。
3. **收编**：主卡散落的 `all_cooling_fast_fail`、`affinity_enabled` 移入对应齿轮卡；
   `priority_in_balanced` 留在负载均衡卡（与模式按钮同卡，语义内聚）。
4. **吸收层 noEffect 警告已有**（settings-page.tsx:2808-2812），保留 —— 它正是
   「线上这组旋钮是死旋钮」的诚实提示，简化后依然重要。
5. 切档时 toast 文案补「已应用智能调度参数矩阵（高级参数里被显式覆盖的项除外）」。

### 4.3 兼容性

- **旧配置零变化**：Manual 默认不变；所有键保留解析与消费；矩阵只影响「显式切档」。
- **`config.example.json`**：补 `throttleProfile` 三档注释 + 智能档矩阵说明
  （现在 example 连 throttleProfile 都没有，blockers-config.md §6 已指出）。
- **文档**：`CLAUDE.md` 的档位说明同步；`docs/absorb-layer-design.md` 引用矩阵。

### 4.4 验证

- 服务器 Docker「验证循环」跑 `cargo test --no-default-features`（新增矩阵测试 +
  既有守卫全部通过）。
- 前端 `pnpm build` + 手动点三按钮 → 检查 PUT /config 载荷中的 20 个字段值。
- 线上切换（nbus）按「skiapi 构建 release → 中转 → nbus 替换」流程，先备份。

---

## 5. 自 review：每个被隐藏旋钮的「为什么用户不需要碰」论证

### 5.1 D 级（完全隐藏）

| 旋钮 | 为什么不碰 |
|---|---|
| `self_heal_max_shift=4` | 只防 `2^n` 位移溢出（config.rs:995-999）：60×2⁴=960 已超上限 900，消费点另有 31 硬 clamp 兜底。任何比 4 大的值都被上限 900 吃掉；比 4 小的值没有任何场景论证。纯防 panic 参数。 |
| `rpm_reserve_slots=0` | 与「无此功能」等价；语义是与 headroom 叠加的固定名额预留，但 headroom 85% 已覆盖突发缓冲需求，双保险无实测依据。隐藏后保留解析。 |
| `upstream_retry_absorb_suspended=false` | 吸收 403 风控 = 把必然失败推迟再返回：风控窗口约 10 分钟 ≫ 任何单请求预算，且与自愈 60s 退避直接冲突（15s 内重打同账号抵消自愈）（config.rs:327-333）。唯一例外是同时设 swap_budget——那已经是部署侧决定，不属用户旋钮。 |
| `upstream_retry_absorb_server_error=false` | 外挂实测 11.6 次重试才救回 1 个请求（config.rs:342-344）——「不分机理一律重试」的账单就是它。5xx 与 429 机理不同（可能是上游整片故障），默认关是显式决定。 |
| `upstream_retry_absorb_capacity_400=false` | 判据只认 `MODEL_TEMPORARILY_UNAVAILABLE/INSUFFICIENT_MODEL_CAPACITY` 两个谓词，认裸 `ThrottlingException` 会把真限流拖进容量路径（config.rs:359-363）。且 provider 内部已有容量慢速重试，本开关只是第二层。 |
| `prompt_cache_ttl_seconds=3600` | **死配置**：全仓零读取点（blockers-config.md 1.3 实锤，config.rs:744-748 自述）。现行估算是无状态重算，没有需要按时间过期的缓存表。改成什么都不影响行为。 |
| 3 个 86400s 长冷却硬窗 | 不是旋钮：认证失败/账户暂停/配额耗尽的冷却时长硬编码在 cooldown.rs:127-129，永不自动恢复是**刻意设计**（防死号反复试探）。scale 明确不碰它们（config.rs:499）。没有配置对应，也就不存在「隐藏」——本文档只是确认它们不该有旋钮。 |

### 5.2 C 级（折叠进高级参数）

| 旋钮 | 为什么不碰 |
|---|---|
| `cooldown_scale_pct=100` | 全局缩放短时冷却（15s/20s/30s 那档），语义是「号多调小、号少调大」。但「号多少」的用户直觉映射不到精确百分比；真正的冷却行为差异由**档位**承载（智能=100 / 稳定=150）。想微调的高级用户仍可在折叠区改。 |
| `all_cooling_fast_fail=true` | 全池冷却时立即 429 + Retry-After 让客户端退避，比网关内硬扛温和且减少对被风控号的零星试探——两个方向都对，没有任何场景论证关掉更好。 |
| `self_heal_max_backoff_secs=900` | 与 403 突发窗口（约 10 分钟）同量级（config.rs:983-985），60s 起翻倍一窗内最多探两三次。调大只加长故障沉默，调小加深封禁风险，两侧都无收益。 |
| `inbound_throttle_enabled=true` | 入站整形的存在理由（削峰保护号）在所有部署形态下都成立；关掉 = 突发直接打上游，没有任何保护性收益。 |
| `inbound_target_rpm=100 / min=20 / max=300` | AIMD 的初值与边界。实测已证明 AIMD 会自己收敛（线上 true 且稳定运行）；初值只影响启动后前几分钟的收敛路径，边界值 20-300 覆盖所有合理容量。 |
| `inbound_burst_secs=2` | 允许 2 秒突发不排队——纯体验参数，2s 是「短时小突发」的合理语义，改 1/5 无场景论证。 |
| `inbound_queue_max_wait_secs=30` | 排队最长 30s 后放行（智能档）——与吸收预算 45s 同量级，排队超过 30s 说明已堆积，继续排无意义。 |
| `rpm_headroom_factor=85` | 预留 15% 缓冲让饱和判定早于上游硬限，削 60s 滑窗边界爆发——85 是「接近但不贴顶」的合理值，且 0/100 语义=不打折已被记录（config.rs:653）。调它的唯一场景是上游限额极紧/极松，属高级诊断。 |
| `rpm_hard_gate_overload_wait=false` | 整池饱和时回退软门「选最不坏的号继续」而不是阻塞等待——保守默认正是「不阻塞」。开它要理解背压语义，无收益场景。 |
| `rate_limit_daily_max=500 / min_interval=1000 / jitter=20` | 拟人限流的细参，只在 L1 开启后生效（默认关）。500/日、1s 间隔、20% 抖动就是「像人」的默认节奏；改它们的前提是已经判断「要模拟人类节奏」，那一步本身才是用户决策。 |
| `balance_weight_enabled=true / floor=50` | dwgx 真机观察余额拉平（config.rs:1383-1391），是「长期把号池额度拉平」的净收益机制；floor 50 的因子区间 [0.5,1.0] 让差 10~20% 属微调不喧宾夺主。关掉 = 退回纯 0.7.23 行为，无收益。 |
| `health_429_weight_enabled=true` | 429 EWMA 降权就是既有 health 机制本身（config.rs:680-683）——关掉 = 让偶发 429 不影响分流，但偶发 429 恰恰是号健康的信号。默认开。 |
| 吸收层 `min_delay=150ms / max_delay=15s` | 号池亚秒级恢复不该睡满 1s（shield p50 偏高的病根之一）；15s clamp 与外挂一致。两者都是实测推导值，微调无数据支撑。 |
| `upstream_retry_absorb_swap_budget_secs=0` | 非零 = 单条客户端请求最长可占用连接数分钟（换号空窗 10 分钟场景）——「要不要让客户端长挂等补号」是部署侧决定（config.rs:385-388），不该由升级或按钮悄悄带来。 |
| `upstream_concurrency_limit=16 / per_credential_limit=8` | 防上游重试放大的硬闸，Semaphore 构造时固化（重启生效）。16/8 是「至少两号能同时打满」的比例（kiro2cc 对照 50/20 同为 40% 量级，config.rs:587-588）。调它需重启 + 理解并发语义，折叠区外更合适。 |
| `inbound_rpm_auto=true` | 见 §1.2：语义陷阱（AIMD 单向棘轮），**不该裸给用户**。智能档置 true 交给后台；想彻底关掉的人必须懂棘轮问题，放高级折叠区 + 警告文案。 |
| `inbound_queue_timeout_passthrough=true` | 见 §1.2：语义陷阱。已被档位接管（Direct=true/Shielded=false），折叠区保留给「介于两者之间」的专家场景。 |

### 5.3 保留的 B 级（折叠区但保留主入口或明确入口）

`auto_disable_suspicious`（安全阀，风控自动禁用防加重封禁——有人想要手动关）、
`self_heal_base_backoff_secs`（60s 起翻倍，个别部署想缩短探测间隔）、
`credential_rpm_limit`（全局每号 RPM 软上限，per-cred 覆盖的默认值）、
`rate_limit_enabled`（拟人限流总开关——防关联场景的显式决策）、
吸收层 A1/A2/A3/A10（总开关/预算/轮次/终态码——「吸收」行为的核心四参）、
`affinity_enabled`（会话亲和）、`custom_api_first`（跨池优先级）、
`overload_fallback_model`（容量回退模型）。这些保留在折叠区 + 各自齿轮卡，
不占主界面，但可被搜索到（settings-page 的搜索态强制展开机制已存在）。

---

## 6. 风险与边界（诚实披露）

1. **吸收层矩阵值对线上无效**：线上全走透传，智能档的 `absorb_enabled=true` 写入
   后不生效（面板有 noEffect 警告）。这不改变方案——矩阵是「切回 Kiro 主路径时
   的正确默认」，且透传形态下用户本来就不该碰这组。
2. **`cooldown_enabled` 矩阵值 true ≠ 线上现状（false）**：这是**故意的修正**——
   线上 false 是语义陷阱（429 后坏号不退避）。但「切智能档会改线上冷却行为」这个
   事实必须在按钮确认框里说清楚，不能让用户无感切换。
3. **`inbound_rpm_auto` 棘轮问题未修**：矩阵置 true 是「后台逻辑智能」的方向，
   但 AIMD 单向棘轮（config.rs:1869-1873 注释）本身是已知缺陷，不在本方案范围
   （修它是另一个波次）。注释修正照做，行为修正另立任务。
4. **本调研未跑任何测试/构建**（只读任务）；行号基于当前工作树，改动后需重核。
5. **矩阵覆盖 20 字段是行为变更面**：实施时必须跑通
   `throttle_profile_never_overrides_explicit_keys`（显式优先契约）和新增矩阵测试，
   服务器 Docker 验证循环见 CLAUDE.md。

---

## 7. 结论

36 个调度旋钮 → 3 个按钮 + 1 个高级折叠区。A 级 5 个留在主界面（负载均衡模式、
优先级分层、冷却主开关已在内），B 级 9 个在折叠区可搜到，C 级 15 个 + D 级 7 个
被隐藏（默认值即正确，论证见 §5）。后端改动集中在 `apply_throttle_profile` 矩阵
扩展 + 注释修正 + restore 表补 5 项；前端改动集中三按钮 + 去三处重复 UI。
旧配置零变化（Manual 默认 + 只填空不覆盖契约不动），验证走服务器 Docker 循环。
