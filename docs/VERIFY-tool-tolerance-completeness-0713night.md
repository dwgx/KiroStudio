# 工具调用容错 6 开关 · 修复完整性核实报告

> 0713night 三 subagent 深核 + 主循环亲自复核关键时序。逐行读实际代码,不凭注释。
> 结论一句话:**6 项功能主体兑现、全栈接线零断点,但查出 5 个真实缺陷**(1 个默认配置下就中的时序 bug + 2 个开关拆开即漏 + 2 个清洗缺口)。

---

## 一、总评

| 项 | 承诺兑现 | 接线 | 结论 |
|---|---|---|---|
| ① JSON 修复层(默认开) | ✅ 6 项全兑现 | ✅ | 无 bug,流式+非流式均接,测试充分 |
| ② 拼装非法对齐失败态(默认开) | ⚠️ 主体兑现 | ✅ | 与③拆开有洞(见缺陷2/3) |
| ③ 如实暴露错误(默认开) | ⚠️ 主体兑现 | ✅ | 与②拆开有洞 + 收尾补发位置描述不符(实际在 handler 不在 generate_final_events) |
| ④ 清洗泄漏 token(默认开) | ❌ 有缺口 | ✅ | 缺 court/card/call + 无 Claude 门控 + 死条目(见缺陷4/5) |
| ⑤ 截断跨轮恢复(默认关) | ✅ 决策逻辑兑现 | ✅ | 决策正确,但残留场景命中时序 bug(缺陷1) |
| ⑥ 工具描述上限(默认10000) | ✅ 4 项全兑现 | ✅ | 无问题,多字节安全 |

**全栈接线**:config→main→router→handlers/converter→mod→admin types→service→api.ts→settings-page,6 项**每一环都在**,前端 `?? 默认值` fallback 与后端 default **逐项一致**(diff 基线正确、热更真生效)。**零断点**。

**隔离铁律守住**:②③⑤ 置失败态全程只改 `self.completion`,**绝不 report_failure 连坐号**(completion 只流向 usage 记账,从不回流 token_manager 健康/冷却/family)。已逐行坐实。

---

## 二、查出的 5 个真实缺陷(按严重度)

### 🔴 缺陷1(最严重·默认配置下就中):无 stop 残留截断 → error 事件漏发,客户端误判成功
**现象**:上游干净 EOF 截断、某 tool_use 从未收到 `stop`(非 decoder_stopped、非 transport_error)时:
- `handlers.rs:896` 检查 `!completion().is_ok()` 决定补发 error —— 此刻 completion 还是 **Ok**(残留 flush 尚未跑);
- `handlers.rs:903` 才调 `generate_final_events()`,其内部 `stream.rs:1781-1791` 才 flush 残留 tool 缓冲,flush 内(1735/1712)这时才把 completion 置失败;
- 收尾已 return,**无二次检查** → **error 事件永久漏发**。
**净效果**:客户端收到 `input:{}` 的 tool 块 + 正常 `message_stop` = **误判成功**,服务端却记为失败。**默认 ②开③开 下就成立**,`/cc/v1` 缓冲路径(handlers.rs:1669 先于 1676)同构。
**注意**:收到 stop 的正常路径无此问题(flush 在 chunk 处理期就置态,早于 None 分支)。仅"无 stop 残留"这一支受影响。
**修法方向**:收尾把「残留 flush」提到「error 检查」之前——先 `generate_final_events()`(触发残留 flush 置态),再检查 completion 补发 error。或在 flush 后二次检查 completion。两条路径(/v1 896、/cc/v1 1669)都要改。

### 🔴 缺陷2(②开③关):记账=失败却仍发坏 JSON,自相矛盾
**控制流**(`stream.rs` `flush_tool_input`):`if !repaired_ok { ⑤ ; ②(1735 置态不return) ; ③(1744 关则不return) }` → fall-through 到 1758 `handle_content_block_delta` **把坏 JSON 原样发出**。
**净效果**:② 记了失败态,③ 关又没拦住坏 JSON → 客户端 parse 失败报 Invalid tool parameters,收尾(896)又因 completion 非 ok 追加 error → 客户端**同时**收到坏 delta + 尾部 error,自相矛盾。且与非流式(干净 502、永不发坏内容)不一致。

### 🔴 缺陷3(②关③开):吞坏 JSON + 记成功 + 不发 error → 客户端把空参当成功执行
**控制流**:② 关(completion 保持 Ok)→ ③ `return Vec::new()`(不发坏 JSON,但 `content_block_start` 已发过 `input:{}`,收尾照发 `content_block_stop`)→ 收尾 896 因 **completion 仍 Ok** → 不补发 error,`record.outcome=Success`。
**净效果**:客户端得到 `input:{}` 的 tool_use 且判定成功 → 按"无参成功调用"执行(`flush_tool_input` 注释 1647 自陈"比报错更危险")。

> 缺陷2/3 共同根因:②③ 是**两个独立 AtomicBool + 独立 admin 热更**(service.rs:1207),代码注释多处写"与③配对/与②配对",但**无任何代码强制联动**。默认双开自洽,admin 手动拆开即漏。
> **修法方向**(方案已列 P2-2):把③的"不发坏 JSON"条件从 `tool_expose_error_to_client_enabled()` 改为 `completion.is_err()` —— 失败态即不发坏 JSON,与失败态绑定,消除所有拆开组合的矛盾。

### 🟡 缺陷4(④清洗缺高频 token):court/card/call 全缺失
`LEAKED_CONTROL_TOKENS`(`stream.rs:1183`)= `["course","count","care","課","课","coursecount"]`。真实日志最高频的 **court(202 次,全独占整行)** 及 card/call **全不在清单** → 描述承诺的"清洗"对最高频 token **完全失效**。

### 🟡 缺陷5(④清洗无 Claude 门控 + 判据过宽):正在误删 Claude 正文
- `clean_leaked_tokens`(`stream.rs:1232`)只受开关门控,**无** Claude 系模型排除(对比 `strip_dsml_markers:1090` 有)。`config.rs:208` 注释自认"对所有模型可用(含 Claude 路径)"。
- 判据 `strip_leaked_prefix:1220` 实为 `if c.is_whitespace() || c.is_ascii_lowercase() { 不剥 } else { 剥 }` —— **除空格和 ASCII 小写外一律剥**(大写/数字/标点全触发),比描述的"后接 CJK/全角/冒号"宽得多。
- **后果**(默认开,打到主力 Claude 路径):`count: 42`(日志/列表)→ 剥成 `: 42`;`countDown()`(代码)→ 剥成 `Down()`;`courseCatalog` → 剥成 `Catalog`;`care2share` → 剥成 `2share`。**这是当前线上正在发生的对 Claude 正文的误删。**
- 次要:`coursecount` 被 `course` 前缀遮蔽(1212 顺序遍历),永远不可达 = 死条目。
> **修法方向**(方案 P0-1):先加 Claude 门控 + 判据收严(仅紧邻 CJK 才剥);court 单独处理"独占整行"特例;删死条目。

---

## 三、测试覆盖缺口(诚实标出)
- ②③ 的开关组合(②开③关/②关③开)+ 残留收尾时序:**零测试**(stream.rs:2960 注释因进程级 static 并行污染故意不测开关切换态)。缺陷1/2/3 **无回归网兜底**。
- ⑤ 触发后的实际收尾行为(是否真补发 error、残留是否漏发):纯决策函数 `should_recover_truncation` 覆盖完整,但端到端**无测试**。
- unwrap 的"关 repair 连带关解包"耦合(P2-1):无测试固定。
- 非流式 repair+unwrap 接线:靠共享纯函数间接覆盖,"非流式确实调了"这条接线本身未被测试锁死。

---

## 四、诚实边界
- 以上均为**网关侧可修**的完整性缺陷(不是模型侧 Bug B)。缺陷1/2/3 是控制流/时序,缺陷4/5 是清洗覆盖与门控。
- ① JSON 修复层、⑥ 描述上限、全栈接线 —— **确认健康,无需动**。
- 修复优先级:缺陷1(默认就中)≈ 缺陷5(正在误删)> 缺陷2/3(需 admin 拆开才中)> 缺陷4(功能缺失非错误)> P2-1(排查时才影响)。
