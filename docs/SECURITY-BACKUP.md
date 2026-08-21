# 备份与密钥安全

> 本文档约束**备份策略**,不涉及运维脚本改动——只说明安全边界与应遵循的规则。

## 现状

- `credentials.json` / `trash.json` 可开启 at-rest 加密(`encrypt_credentials_at_rest` 配置)。
- 加密密钥 `.at_rest.key`(32 字节,0600)与凭据文件**同目录**、独立成文件(`src/common/secret_store.rs`)。
- 数据位置:Linux 走 cwd/exe 目录逻辑(默认 `credentials.json` 所在目录);Windows 为 `<exe>/KiroStudio-data/`。

## 核心风险:密钥与密文同进备份包 = 备份泄露即凭据全解

at-rest 加密的设计前提是「密文被单独拷走解不开」。但**整目录打包备份**会把 `.at_rest.key`
和密文一起带走——此时加密形同虚设,备份泄露 = access_token/refresh_token/api_key/proxy_password
全部可解密复用。

## 规则

1. **密钥必须独立于凭据备份**。备份包可以包含 `credentials.json`(密文),但 `.at_rest.key`
   必须单独保管(如密码管理器),绝不允许进凭据备份包。
2. **备份脚本排除 `.at_rest.key`**(若现有备份脚本是整目录打包,先加排除再备份)。
3. **密钥丢失 = 密文永久解不开**。删除/迁移密钥前,先通过设置页导出**明文**凭据(导出走明文,
   与 at-rest 加密无关),再重新导入。
4. **Windows 无 ACL 保护**:NTFS 限制下密钥文件权限收紧是 no-op,任何本地进程可读,
   备份前务必确认 `.at_rest.key` 是否被带进包。

## 泄露场景对照

| 场景 | 是否失守 | 说明 |
| --- | --- | --- |
| 密文文件被单独拷走/误传 | 否 | 机器绑定 + 密钥分离,解不开 |
| 整目录备份/硬盘送修/打包误传(含密钥) | **是** | 密钥与密文同包,等于明文 |
| 本机进程读取(Windows 或同账号进程) | 是 | 设计边界:at-rest 不抗本机攻击者 |

## 相关文件

- `src/common/secret_store.rs` — at-rest 加密实现与密钥文件管理
- `src/model/config.rs` — `encrypt_credentials_at_rest` 开关
- 前端 adminKey 会话存储说明见 `admin-ui/src/lib/storage.ts`(sessionStorage,关标签即清)
