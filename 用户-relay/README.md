# 下游用户 Key Relay 部署说明

这套服务部署在下游用户自己的服务器上。它接收卖方服务发送到 /push 的 Key，然后把 Key 导入用户自己的 kiro-rs。

服务不会购买 Key，也不会把用户的 kiro-rs 管理员 API key 发送给卖方。卖方只需要知道公开的 /push 地址和 secret。

## 1. 准备文件

将以下三个文件放到同一目录：

    relay.js
    config.json
    README.md

要求 Node.js 16 或更高版本，不需要安装第三方依赖。

## 2. 修改 config.json

    {
      "host": "0.0.0.0",
      "port": 8285,
      "secret": "替换成随机且至少 24 个字符的密钥",
      "kiroServer": "http://127.0.0.1:8990",
      "kiroApiKey": "填写你自己的 kiro-rs 管理员 API key",
      "region": "us-east-1",
      "kiroTimeoutMs": 15000,
      "deliveryLogFile": "./delivery-log.ndjson"
    }

说明：

- host：有 HTTPS 反向代理时建议监听 127.0.0.1；直接对外提供端口时使用 0.0.0.0。
- port：relay 监听端口，示例为 8285。
- secret：只提供给卖方服务，不要与 kiroApiKey 使用同一个值。
- kiroServer：用户自己的 kiro-rs 基础地址，不要填写 /api/admin/credentials。
- kiroApiKey：用户自己的 kiro-rs 管理员 API key，只保存在本机 config.json。
- region：默认使用 us-east-1，如实际区域不同请按账号配置。
- deliveryLogFile：只保存 delivery_id、Key 摘要和导入结果，不保存明文 Key。

可以使用下面命令生成随机 secret：

    openssl rand -hex 24

## 3. 启动

直接启动：

    node relay.js

推荐使用 PM2：

    pm2 start relay.js --name key-relay
    pm2 save

健康检查：

    curl http://127.0.0.1:8285/health

预期返回：

    {"ok":true,"service":"key-relay","target":"kiro-rs"}

## 4. 提供给卖方的信息

只提供下面两项，不要提供 kiroApiKey：

    Relay URL: https://你的域名或IP:8285/push
    Relay secret: config.json 中的 secret

如果使用 HTTPS 反向代理，把 URL 换成 HTTPS 地址。卖方服务会向该地址发送包含 secret、key、region、delivery_id 和 key_sha256 的 JSON 请求。

## 5. 安全要求

- 不要把 config.json 提交到公开 Git 仓库。
- 不要把 kiroApiKey 发给卖方或放到 relay URL 中。
- /push 最好只通过 HTTPS 对外开放。
- 防火墙只放行卖方服务器的来源 IP 更安全。
- 日志不会输出完整 Key，但仍应限制日志文件访问权限。
- 相同 delivery_id 和相同 Key 的成功请求会返回 duplicate，不会再次调用 kiro-rs；相同 delivery_id 携带不同 Key 会返回 409。
- 用户删除或轮换 kiro-rs 管理员 API key 后，需要同步更新本地 config.json 并重启 relay。

## 工作流程

    卖方购买 key
        -> 卖方保存 key
        -> 卖方持久化待分发任务并自动重试
        -> 卖方调用下游用户的 /push
        -> relay 验证 secret
        -> relay 校验 delivery_id 和 Key 摘要
        -> relay 调用用户 kiro-rs /api/admin/credentials
        -> key 导入用户 kiro-rs
