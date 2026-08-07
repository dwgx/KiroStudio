# 分身管理 + shield 内置化 —— 规格与审查（2026-08-03）

11 路 agent 的产出。**先读 `REVIEW-adversary-9-BLOCKERS.md`。**

## 阅读顺序

1. **`REVIEW-adversary-9-BLOCKERS.md`** —— 对抗审查，抓出 9 个 BLOCKER（4 个致命）。
   ⚠️ **三份 SPEC 不能直接实施**，先看这份知道哪里会炸。
   它也明确列了"防守成立"的部分，别把那些当问题重做。
2. `REVIEW-consistency.md` —— 前后端契约比对 + 用户诉求逐条核对（找"有没有漏"）。
3. `SPEC-backend.md` / `SPEC-frontend-clone-mgmt.md` / `SPEC-shield-builtin.md` —— 三份设计。
   ⚠️ 它们在传给审查 agent 时被 `.slice()` 截断过，所以审查只覆盖了它读到的部分。
4. `recon-*.md` —— 六份侦察，含精确 file:line 与 patch 草案。

## 已知缺口（审查指出）

- `CredentialSnapshot` 这个类型**在仓里不存在**，SPEC-backend 的 patch anchor 套不上。
  面板实际收的是 `CredentialStatusItem`（`src/admin/types.rs:24`，构造在 `service.rs:384`）。
- 用户诉求 #4（推号 + 自动分身，**默认必须关**）在三份 SPEC 里**整节缺失**。
- 余额扇出**漏了两处**：`refresh_all_balances_gently` 绕过缓存、乐观修正按 id 叠加
  → 用户报的"同账号百分比不一致"按 SPEC 改**修不掉**。

## 结论

交接文档（`HANDOFF-2026-08-03.md` 第 6.4 节）的建议是：
**不要再派一轮 agent，直接按对抗审查的意见自己写并实施。**
审查已经把该改什么说得足够具体，而三份 SPEC 有实质缺口。
