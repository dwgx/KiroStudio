# 接手文档 · 2026-08-08（上下文压缩交接）

> 本文件在上下文即将压缩时由助手自己写，记录当前状态与后续要点。
> 状态入口仍是 `STATUS.md`；长期约束仍是 `CLAUDE.md`。

---

## 0. 一句话结论

master 已到 **`23e1be2`**（git status 干净，工作树无未提交），并已用 **`kirostudio-hotswap deploy`** 部署到**新机 `143.20.230.62` 容器**（镜像 `local-23e1be2-20260807-231732`，容器 healthy）。**旧机 ws-vps（143.20.230.248）已退役，别再用它部署。**

---

## 1. 当前代码状态（master 提交链）

```
23e1be2 feat: 代挂模型探测/白名单gate + 回收站分页 + 日志暂停持久化   ← HEAD
fb3e1fa feat: 代挂协议修复开关与清理保护
b13c6b4 fix: deepseek 归一化补全（模型重写/多轮thinking注入/output_config白名单）
486cb39 feat: 工具协议双向映射与 reasoning 签名回传
b0fd64b feat: 端点换桶/克隆分身/运维优化多主题合并
```

工作树 **0 未提交**（`git status --short` 为空）。当前分支 `master`。

---

## 2. 本轮完成的功能（按提交分组）

### 2.1 端点换桶（b0fd64b）
- `cli-runtime` 端点（`runtime.{region}.kiro.dev` 的 CLI 协议）+ 429 自动换桶（q.* 优先、runtime.* 回退，provider 端点桶 30s 封禁换下一 host）。`credentials.rs::effective_endpoint_order`、`provider.rs::select_endpoint`/`endpoint_buckets`。
- 紧凑行右键菜单限高修复、运维页 SSE 超时 + 日志批处理节流、设置页导航单行。
- **已提交并推送 origin**，含其他会话工作（deepseek 归一化、ops-page 等）。

### 2.2 工具协议双向映射 + reasoning 签名（486cb39）
- **CC↔Kiro 工具名/参数双向映射**（`converter.rs`）：Write→fs_write 等 8 个内置工具、file_path→path、old_string→oldStr、offset/limit→start_line/end_line，出站还原（`map_tool_input_from_kiro`），`set_tool_compat_mapping(false)` 可关。
- **reasoning 真签名回传**（P3-1）：消费上游 `reasoningContentEvent.signature`，thinking 块回传真签名。
- **单图 base64 8MiB 上限**（P3-4）。
- **`deepseek_normalize.rs`**（用户加的 fuckopencode 复刻）修复：模型名重写为 deepseek-v4-flash、injectMissingThinkingBlocks、output_config 空对象/非字符串 effort 删除（**已在 b13c6b4 单独提交**）。

### 2.3 代挂禁用 + deepseek 开关（fb3e1fa）
- **代挂凭据绝不自动禁用**：`record_passthrough_result` 任何 outcome 不写 disabled；`is_entry_selectable` 第二道排除 custom_api。
- **deepseek 归一化开关更新接口**：`POST /credentials/{id}/deepseek-normalize` + 面板 custom_api 设置区开关。
- **清除已禁用排除代挂**：`cleanup_verdict` 第一道 `SKIP_CUSTOM_API`。

### 2.4 代挂模型探测 + 回收站分页 + 日志持久化（23e1be2）
- **代挂探测上游模型**：`GET /credentials/{id}/upstream-models`（`passthrough.rs::fetch_upstream_models`，兼容 OpenAI 三种返回格式），前端 custom_api 设置区「探测上游模型」按钮 + checkbox 勾选 + 保存 allowed_models；`select_custom_api` 加 model 参数 + `allows_model` gate。
- **回收站分页 + 搜索 + 每页 100/200**（settings-page TrashCard，localStorage 持久 `ops.trash.pageSize`）。
- **日志暂停 localStorage**（ops-page `ops.logviewer.live`）。

---

## 3. 🔴 部署信息（关键，别再搞错机器）

- **新机**：`143.20.230.62`，SSH `-p 673 -i ~/.ssh/id_ed25519_pcs_root root@143.20.230.62`。容器化（`/opt/skiapi/docker-compose.yml`），KiroStudio 容器 `skiapi-kirostudio`。
- **旧机 ws-vps（143.20.230.248）已退役**——我一度 hotswap 到旧机导致用户看不到新功能，教训深刻。
- **部署命令**（在新机跑）：
  ```bash
  kirostudio-hotswap deploy master   # 拉代码→编译→新镜像→热更新（shield 吸收空窗）
  kirostudio-hotswap rollback        # 回滚到上一镜像
  kirostudio-hotswap status          # 查看版本
  ```
- **前端 dist**：新机无 node，`admin-ui/dist` 需本地 `pnpm build` 后 scp 到 `/opt/kirostudio-src/admin-ui/dist`（**注意 scp -r 会嵌套成 dist/dist，需修正**）。
- **代码到新机**：本地 `git push` 到新机裸仓 `ssh://root@143.20.230.62/srv/git/KiroStudio-skiapi.git`（master），然后新机 `kirostudio-hotswap deploy`。
- **完整流程文档**：`ws-vps/docs/13-kirostudio-hotswap.md`（新机容器化 hotswap）。

**当前部署**：`local-23e1be2-20260807-231732`（healthy），回滚点 `local-b13c6b4`。

---

## 4. 关键边界与已知问题

- **响应侧 thinking 过滤未实现**：deepseek 归一化只做请求侧；thinking disabled 时上游仍吐 thinking 块，客户端可能报 "Tool result missing"（架构性，需解析 SSE 流）。
- **add-credential-dialog 无模型探测**：当前只在设置弹框可探测/勾选，添加上号弹框未加。
- **fuckopencode OpenAI 层未移植**：KiroStudio 只复刻 deepseek.ts 的 Anthropic 请求侧归一化，未做 OpenAI↔Anthropic 转换。
- **设置页横排**：已做（flex-nowrap）。
- **hotswap 说明书**：在 `ws-vps/docs/` 不在主仓。

---

## 5. 后续建议

1. **add-credential-dialog 加模型探测**（创建时即可探测/勾选，当前仅设置弹框）。
2. **响应侧 thinking 过滤**（如果用户遇到 "Tool result missing"）。
3. **P2 遗留**：0.7.46 未打 tag（OTA 升不到）、deploy/vps 分支合并。
4. **健康标记路径**：容器化后 `/usr/local/bin/kirostudio.health` 只读（文档已知问题，应改写到 `/app/data`）。
