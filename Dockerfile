# syntax=docker/dockerfile:1
# ============================================================================
# KiroStudio 多阶段构建
#   阶段 1 (frontend-builder): Node 构建 React admin-ui 静态产物
#   阶段 2 (builder):          Rust + musl 静态编译，仅用纯 Rust 的 rustls
#   阶段 3 (runtime):          极小 alpine 运行镜像，单二进制 + 前端已内嵌
# 产物：/app/kirostudio（前端经 rust-embed 编入二进制，无需额外静态文件）
# ============================================================================

# ---- 阶段 1：前端构建 ----
FROM node:22-alpine AS frontend-builder

WORKDIR /app/admin-ui
# 先只拷贝依赖清单，命中 Docker 层缓存（源码变动不必重装依赖）
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml admin-ui/.npmrc ./
# 固定 pnpm 大版本与 lockfile(lockfileVersion 9.0)匹配:不 pin 会拉到 pnpm v10+,
# 它对未批准的 build 脚本(@swc/core/esbuild)在 --frozen-lockfile 下硬失败(ERR_PNPM_IGNORED_BUILDS),
# 导致 docker 一键构建当场崩(fresh clone 实测)。pnpm@9 与本 lockfile 一致且行为稳定。
RUN npm install -g pnpm@9 && pnpm install --frozen-lockfile
COPY admin-ui ./
RUN pnpm build

# ---- 阶段 2：Rust 静态编译 ----
FROM rust:1.96-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# 内嵌前端产物（router.rs 里 #[folder = "admin-ui/dist"]）
COPY --from=frontend-builder /app/admin-ui/dist /app/admin-ui/dist

# 关闭默认 native-tls，仅用纯 Rust 的 rustls（配 config.json 的 "tlsBackend": "rustls"）
# alpine/musl 下省去 OpenSSL 编译，构建更快、镜像更小
RUN cargo build --release --no-default-features

# ---- 阶段 3：运行镜像 ----
FROM alpine:3.21

# ca-certificates：出站 HTTPS 校验证书链；curl：HEALTHCHECK 探活
RUN apk add --no-cache ca-certificates curl \
    && addgroup -S app && adduser -S -G app app

WORKDIR /app
COPY --from=builder /app/target/release/kirostudio /app/kirostudio

# 配置与凭据挂载点（宿主机 ./config 映射到此）
VOLUME ["/app/config"]

EXPOSE 8990

# 容器内固定监听 8990（宿主端口由 docker-compose 的 ${KIROSTUDIO_PORT} 映射）
# /admin 是 Admin UI 首页，返回 200 即视为存活
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8990/admin >/dev/null || exit 1

USER app

CMD ["./kirostudio", "-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]
