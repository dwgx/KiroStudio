# 错误码/提示词可配置化 —— 机制研究（只读，不改代码）

> 2026-08-15 研究落盘。目标：为「错误码/提示词可配置化」设计提供机制依据。
> 全部结论带源码行号（工作树当前状态）。只研究不改代码。
> **实现状态：设计已按本机制落地（W7-W8），errorMessages 表走 TIER1 范式（与本文档结论一致）。**

## 0. 一句话结论

- 我们已有**成熟的三层热重载机制**（TIER1 ArcSwap / TIER2 任务重挂 / TIER3 进程镜像），
  加一个新配置项有固定的 7 处接线点，全部有先例可抄。
- **大表（错误码表）的最佳范式已有现成先例**：`model_mapping`（HashMap 整表替换 +
  每请求 ArcSwap 快照读），不是 TIER3 镜像——这决定了错误码表走 TIER1 范式。
- config 更新是「**API 字段级 merge + 落盘整文件替换**」：merge 点在 Rust struct，
  错误码表作为新字段进 `UpdateConfigRequest`，天然获得字段级安全更新，无需另做
  表级 merge。
- **三个参考仓（k2cc / zyphr / NewAPI）都没有下游错误文案可配置功能**——k2cc/zyphr
  错误 type+message 全硬编码；NewAPI 只有管理面板层 i18n，relay 层错误码是常量、
  message 透传。我们是第一个做的，参考价值只在 NewAPI 的文案目录组织。

---

## 1. 热重载全链路图（以 mock_cache 为主线）

### 1.1 Config 结构范式（src/model/config.rs）

- `pub struct Config`（config.rs:103）：Rust 字段 snake_case，JSON 键 camelCase
  经 `#[serde(rename = "mockCacheEnabled")]` 显式映射（config.rs:756-764 是
  mock_cache 两字段的样板）。
- 默认值模式：`#[serde(default = "default_xxx")]` + 独立 `fn default_xxx()`
  （config.rs:1069-1417 一带成排；`default_mock_cache_read_ratio` 在
  config.rs:1416-1418）。
- 字段注释即 TIER 分类依据（写在字段 doc 注释里，无独立清单文件）：
  - **TIER1 运行时字段**：注释「TIER1 热重载，改 config.json 立即生效」——
    冷却/限流/亲和/吸收层/self_heal/模型映射（config.rs:593、608、697、856、966…）。
  - **TIER2 后台任务字段**：token 预刷新、余额同步间隔——改后 abort+respawn
    任务（service.rs:438、4664-4690）。
  - **TIER3 进程镜像字段**：extract_thinking / cc_auto_buffer / prompt_cache /
    mock_cache / tool 容错 7 开关 / strip_env_noise / native_effort——
    「改后调 handlers setter 即时生效」。
  - **需重启字段**：host/port/proxy/tls/版本串/defaultEndpoint/adminKey 等，
    进 `restart_fields` 且 reload 时被 restore 表覆盖回旧值。
  - **内存态字段**（不进 config.json）：auto_disable_quota_exceeded /
    socks_auto_health（service.rs:4282-4309）。

### 1.2 mock_cache 全链路（每环位置 + 改动成本）

```
环 1  前端 diff → PUT /config
      settings-page.tsx:1903-1918（mock 布尔直比 + 比例取整百分比比较，空串不进 diff）
      credentials.ts:661-666  updateConfig() → api.put('/config', req)
      admin-ui/src/hooks/use-credentials.ts:47  useUpdateConfig（React Query mutate）

环 2  路由
      admin/router.rs:176  .route("/config", get(get_config).put(update_config))

环 3  锁 + 字段级 merge（merge 点 = Rust struct，不是 JSON）
      service.rs:3862-3868  update_config：持 config_write_lock（并发 lost update 防护）
      service.rs:3872-4966  update_config_locked：
        · 磁盘重 load（3886，防覆盖进程外改动）
        · 逐字段 if let Some(v) 分支 + 校验 + 置 changed 标志
          mock_cache：4130-4147（ratio 先 sanitize 再比较再写盘）
        · 备份轮换 + diff 审计：4750-4757（rotate_config_backup 3 代 + diff_json_fields 只记字段名）
        · config.save() 整文件原子写盘：4758-4760（fs_atomic::write_atomic）

环 4  changed 标志 → OR 链 → reload_config
      service.rs:4776-4806  hot_or_display_changed OR 链（mock_cache_changed 在 4791）
      service.rs:4807-4811  reload_config（失败仅告警，下次重启生效）
      ⚠️ 源码守卫钉死：absorb_changed 必须在 OR 链（service.rs:7449 测试）

环 5  TIER3 同步块（镜像 setter）
      service.rs:4880-4885  set_mock_cache_config(config.mock_cache_enabled,
                             config.mock_cache_read_ratio) —— 用已更新的 config 而非
                             req 原值（两字段可能只改一个，setter 要拿完整组）

环 6  reload_config（TIER1 机制，token_manager.rs:2682-2777+）
      · Config::load 从盘重读（2689），解析失败零副作用
      · restore 表：restart-only 字段用 ArcSwap 旧值覆盖回（2697-2729，
        proxy split-brain 根治）
      · 刷新热路径原子镜像（2731-2777）
      · ArcSwap store（config() = load_full，token_manager.rs:2674）

环 7  TIER3 镜像本体（handlers.rs:269-303）
      · static AtomicBool + AtomicU64（0.7f64.to_bits() 存 f64）
      · set_mock_cache_config（277-288）：先写 ratio 再写 enabled（关优先语义）
      · sanitize_mock_cache_ratio（291-296）：NaN/±inf → 0.7，其余 clamp [0,1]
      · mock_cache_config() 读取（299-303）

环 8  消费点（每请求读镜像）
      passthrough.rs:502-531  filter_sse_stream_with / filter_json_bytes_with
      （关闭时 mock_cache=None，filter 零改动原样透传）

环 9  启动播种（main.rs:584-587）set_mock_cache_config(config.mock_cache_enabled,
      config.mock_cache_read_ratio) —— 启动值来自磁盘 config
```

**每环改动成本**（加一个新配置项）：

| 环 | 位置 | 成本 |
|---|---|---|
| 1 | config.rs 字段 + default fn + serde rename | ~10 行 |
| 2 | admin/types.rs UpdateConfigRequest 字段（全 Option） | 1 行 |
| 3 | update_config_locked 分支 + 校验 + changed 标志 | ~10 行 |
| 4 | hot_or_display_changed OR 链 +1 行（有守卫样板） | 1 行 |
| 5 | TIER3 setter 同步块 | ~5 行 |
| 6 | reload_config（TIER1 范式免改；TIER3 才要加 setter） | 0（TIER1）/ ~30 行镜像 |
| 7 | handlers.rs static + setter + sanitize + getter | ~30 行 |
| 8 | 消费点改造（硬编码 → 查表） | 看消费点数量 |
| 9 | main.rs 播种 | ~5 行 |
| F | 前端 form + toForm + diff + UI + i18n 三语 | ~50-100 行 |

---

## 2. config 更新模式结论（整文件 vs merge）

### 现状（已核实）

1. **API 层 = 字段级 merge**：`UpdateConfigRequest` 全部 Option 字段
   （admin/types.rs:1308 起），前端 diff 后只发变更字段（settings-page.tsx:1855-2033）。
2. **落盘 = 整文件替换**：`config.save()` 序列化整份 Config → 原子写盘
   （service.rs:4758-4760）；merge 发生在 Rust struct 上
   （`if let Some(v) = req.xxx` 逐字段写 config 结构），不是 JSON merge。
3. **大表先例 = model_mapping**（config.rs:597-611）：HashMap 整表替换，
   TIER1 范式——存盘 + reload_config 换 ArcSwap；消费点在 provider 每次调用
   `token_manager.config()` 取快照（service.rs:4733-4741 注释明写
   「provider 每次调用时取新快照，只需保存 + reload_config 热应用」）。
   前端侧：JSON 编辑器 + 即时校验 + 整表提交（settings-page.tsx:2006-2033）。
4. 另有一条**整份导入**路径 import_config（service.rs:5017，先校验后写盘）。

### 结论：错误码表怎么进

- **错误码表走 model_mapping 同款 TIER1 范式**：`HashMap<String, ErrorSpec>`
  字段（serde default 空表）+ `UpdateConfigRequest` 加 Option 字段（整表替换）+
  存盘 + reload_config。**不需要 TIER3 镜像**——错误翻译点（handlers.rs 的
  ErrorResponse 构造）每请求从 `token_manager.config()` 取 ArcSwap 快照，
  与 provider 读 model_mapping 同型。错误发生频率远低于每请求，ArcSwap
  load_full 成本可忽略。
- 字段级更新的安全性：PUT 只带变更字段 → 后端 merge 到完整 Config → 整文件
  原子写。表内单条修改 = 前端整表提交（先例同 modelMapping），不会出现
  「表被另一个字段的保存覆盖」——因为并发 PUT 有 config_write_lock 串行化
  （service.rs:3866，lost update 防护有守卫钉死）。
- 备份/审计自动获得：rotate_config_backup 3 代 + diff_json_fields 递归对比
  会记到 `errorCodes` 路径（service.rs:539-569，递归 walk 天然支持嵌套）。

---

## 3. 参考仓做法总结

### k2cc（/tmp/ref-k2cc v2.9.6）—— 错误文案 100% 硬编码

- 错误 type 字符串 inline：handlers.rs:96/109/145/199/780/972/978/1004
  （`"invalid_request_error"` / `"rate_limit_error"` / `"overloaded_error"`）。
- 消息硬编码中文：`("invalid_request_error", format!("模型不支持: {}", model))`
  （handlers.rs:780）、`"消息列表为空"`（handlers.rs:783）。
- `config.example.json`：**零 error/msg 相关键**（全文件 grep 无命中）。
- 结论：无可配置错误消息，无参考价值（反面教材：文案与业务逻辑混在一起）。

### zyphr（/tmp/ref-zyphr v0.7.6）—— 同

- 错误 type 硬编码：openai.rs:112、websearch.rs:582、websearch_loop.rs:379-386。
- `config.example.json` 零 error 键。
- 结论：同 k2cc，无可配置性。

### NewAPI（/tmp/newapi-ref）—— 双层：面板 i18n，relay 不 i18n

- **管理面板/API 层**：i18n 目录（i18n/i18n.go + i18n/keys.go +
  i18n/locales/{en,zh-CN,zh-TW}.yaml）。文案文件结构 = 点分键 YAML：
  `user.username_or_password_error: "用户名或密码错误，或用户已被封禁"`，
  按主题分组（# Auth middleware / # Token / # Channel 等注释段）。
  controller 层合计仅 9 处 i18n.T 调用（oauth/redemption/user）。
- **relay 层（客户端面对的上游错误）**：**不 i18n、不可配置**——错误码是 Go
  常量（relaykit/types/error.go 的 ErrorCode，如 `do_request_failed`），
  message 原样透传上游 err（relay/chat_completions_via_responses.go 等处的
  `types.NewOpenAIError(err, types.ErrorCodeDoRequestFailed, http.StatusInternalServerError)`）。
- 结论：NewAPI 的可配置文案只覆盖管理面板自身错误，**下游协议错误的
  type/message 同样无运行时配置**。可借鉴的只有文案目录组织（集中式点分键 +
  按 locale 分文件 + keys.go 注册表），而我们的面板 i18n 已是三语 JSON
  （admin-ui/src/i18n/resources/{zh,en,ja}.json），比它组织方式更贴近当前架构。

### 参考仓结论

三个参考仓**都没有「错误码/提示词可配置化」功能**。本功能没有可照搬的参考实现，
设计时以「我们的 TIER1 model_mapping 范式 + 前端 modelMapping JSON 编辑器先例」
为唯一机制依据即可。

---

## 4. 设计约束清单

### 4.1 可复用环（零新增机制，照抄接线）

| 机制 | 位置 | 复用方式 |
|---|---|---|
| Config 字段 + serde rename + default fn | config.rs | 新增 `error_codes: HashMap<String, ErrorSpec>` |
| UpdateConfigRequest Option 字段 | admin/types.rs | 加 `error_codes: Option<...>` 整表替换 |
| update_config_locked 分支 + 校验 + changed 标志 | service.rs:3872-4966 | 抄 model_mapping 分支（4736-4741）+ exhausted_status 白名单校验（4107-4119） |
| hot_or_display_changed OR 链 | service.rs:4776-4806 | +1 行（守卫样板：absorb_changed_is_in_hot_reload_or_chain，7449） |
| reload_config 换 ArcSwap | token_manager.rs:2682 | 零改动（TIER1 自动覆盖） |
| 备份 3 代 + diff 审计 | service.rs:519-569 | 自动获得（diff 递归 walk 记 `errorCodes` 路径） |
| 前端 diff → PUT | settings-page.tsx:1855-2033 | 抄 modelMapping JSON 校验提交（2006-2033） |
| 卡片注册 + SectionGate | settings-page.tsx:150-174, 2800+ | CARD_INDEX_DEFS 加一项 |

### 4.2 需新增的环（设计重点）

1. **消费点改造**：handlers.rs 里硬编码的 ErrorResponse 构造（参考 k2cc
   handlers.rs:780 同位置的我们的对应函数）改为「查表 → 未命中回落内置值」。
   这是本设计唯一需要动业务逻辑的环。
2. **默认表**：内置错误表（status/message/type 默认值）作为 Rust 常量/函数
   存在，config 空表 = 全用内置——**必须保证删掉配置键行为不变**（与 mock
   cache 默认关同型的零回归承诺）。
3. **守卫**：翻译点必须查表（防后人把内置值硬编码回去）——参照
   model_mapping 守卫风格。

### 4.3 校验要求（对齐现有模式）

现有校验先例（全在 update_config_locked 内 return Err(InvalidCredential)）：
- 值域白名单：`upstream_retry_absorb_exhausted_status` 只允许 429/503
  （service.rs:4107-4119）
- 枚举白名单：load_balancing_mode（service.rs:4724-4727）
- 数值范围：port 1-65535（3948-3951）、host 非空（3935-3940）
- sanitize 兜底：mock ratio clamp（4142）+ setter 侧二次 clamp（handlers.rs:291）

错误码表需要的新校验（设计建议）：
1. **status_code 白名单**：只允许 400/401/403/404/429/500/502/503/529 等
   Anthropic 生态常见码（或至少 400-599 且排除 499/599），对齐 exhausted_status
   白名单先例。
2. **type 白名单或格式**：Anthropic 协议 error.type 有枚举（invalid_request_error
   / authentication_error / permission_error / not_found_error /
   request_too_large_error / rate_limit_error / api_error / overloaded_error
   / 529 overloaded 等），type 不在枚举应拒绝或回落。
3. **message 长度上限**：如 ≤500 字符，防日志/面板毒化。
4. **Retry-After 范围**：0-3600 秒（若允许配置），非法回落不传该头。
5. **表条目数上限**：如 ≤200 条，防配置膨胀。
6. 校验失败即整表拒绝（Err），不部分接受——与现有字段级校验语义一致
   （改 config 是管理员操作，宁可拒收不可半生效）。

---

## 5. 前端设置页扩展点

### 5.1 高级设置区现有结构

- **分组外壳**：`AdvancedDisclosure`（settings-page.tsx:520-540，默认折叠；
  SectionGate 按 section 控制可见性）。
- **卡片注册**：`CARD_INDEX_DEFS`（settings-page.tsx:150-174）——每卡
  `{ section, titleKey, kwKey }`，kw 为三语逗号分隔同义词，供搜索命中。
  advanced 区现有 4 卡：toolFault / cliAlign / advAbsorb / advCache。
- **卡片样板**（mock cache 卡，settings-page.tsx:2800-2848）：
  `SectionGate section="advanced"` → Card → CardHeader(CardTitle+Highlight)
  → CardContent：Callout 说明 → Field(Switch) → GroupHeading 分组 →
  Field(NumberStepper)。
- **大表样板**（modelMapping 卡，settings-page.tsx:2850-2870+）：
  Callout 三行说明 → textarea JSON 编辑器 → 即时校验
  （modelMappingParsed，settings-page.tsx:2050-2062：纯对象 + 值全 string）
  → diff 时 JSON deep 比较（2021-2032）+ 清空语义（清空编辑区 = 提交空对象删全部）。

### 5.2 新入口添加法

1. `CARD_INDEX_DEFS` 加一项（新卡进搜索索引，否则「搜到的项不可见」bug——
   见 526-527 注释）。
2. 页面 JSX 加一个 `SectionGate section="advanced"` 包 Card。
3. i18n 三语补 titleKey/kwKey + 字段文案（settingspage.* 键）。

### 5.3 dialog 页面化先例（已有，可抄）

- **ops-detail-dialogs.tsx**（1425 行）：4 个完整独立 Dialog 组件——
  `TraceDetailDialog`（269，服务端搜索+分页+防抖+过滤面板+展开态）、
  `UsageDetailDialog`（741）、`TrashDetailDialog`（1036）、
  `BgCacheDetailDialog`（1199）。模式 = 独立组件自带全部状态 + i18n +
  受控 `open`/`onOpenChange` 属性。
- 从设置页打开：settings-page.tsx:1039-1041
  `<TraceDetailDialog open={detail === 'traces'} onOpenChange={(v) => !v && setDetail(null)} />`
  —— 父组件持 `detail` state，按钮置值开、onOpenChange 关。
- 另有 `ConfirmDialog`（ui/confirm-dialog）供确认类弹窗。
- **结论**：错误码表编辑器若不想塞进设置页长表单，可照 TraceDetailDialog
  模式做一个独立 Dialog（设置页高级区放「错误码配置」入口按钮，点击开弹窗，
  弹窗内表格/JSON 编辑 + 即时校验 + 保存走 updateConfig 的 errorCodes 字段），
  无需新路由/新页面框架。

---

## 6. 自 review：机制结论可验证性

| 结论 | 证据 | 验证方式 |
|---|---|---|
| 热重载 9 环链路 | 每环行号如上 | 已逐环读源码；`rg set_mock_cache_config` 全仓仅 2 调用点（main.rs 播种 + service.rs 热更）+ 1 定义 |
| 落盘是整文件替换 | service.rs:4758-4760 `config.save()` | 已读；save 内部 fs_atomic::write_atomic 注释在 rotate_config_backup 说明（516-518） |
| API 是字段级 merge | UpdateConfigRequest 全 Option（types.rs:1308 起）+ update_config_locked 逐字段 if let | 已读 40+ 分支 |
| model_mapping 是整表替换大表先例 | config.rs:597-611 + service.rs:4733-4741 + settings-page.tsx:2006-2033 | 已读三处 |
| k2cc/zyphr 错误文案硬编码 | k2cc handlers.rs:780/783/96-200 等 + config.example.json 零 error 键 | rg 全仓核实（config.example.json grep 无命中） |
| NewAPI relay 不 i18n | relaykit/types/error.go ErrorCode 常量 + relay 层 0 处 i18n.T | rg relay/ 目录 0 命中；controller 层仅 9 处 |
| OR 链有源码守卫 | service.rs:7449 absorb_changed_is_in_hot_reload_or_chain | 已读测试（还有 mock_cache 同款断言的其它守卫） |
| 前端 diff 不提交非法值 | settings-page.tsx:1903-1918（空串跳过）、2006-2033（JSON 校验 + 清空语义） | 已读 |

未验证项（诚实披露）：
- 未跑任何测试/构建（只读任务）；行号基于当前工作树，改动后需重核。
- TIER 分类的依据是字段 doc 注释散点，不是单一清单文件——新增字段时
  「进哪个 changed 标志」依赖人读注释，这是既有设计风险（曾被
  clone_default_enabled 漏 OR 链踩过，service.rs:3903-3907 注释自述）。
- NewAPI relay 层错误响应构造仅抽查 3 个 handler，未全量遍历
  （0 处 i18n.T 已足以支撑「relay 不 i18n」结论）。
