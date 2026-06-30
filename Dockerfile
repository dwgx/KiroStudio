FROM node:22-alpine AS frontend-builder

WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml admin-ui/.npmrc admin-ui/pnpm-workspace.yaml ./
RUN npm install -g pnpm && pnpm install --frozen-lockfile
COPY admin-ui ./
RUN pnpm build

FROM rust:1.92-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY --from=frontend-builder /app/admin-ui/dist /app/admin-ui/dist

# 关闭默认 native-tls，仅用纯 Rust 的 rustls（配 config.json 的 "tlsBackend": "rustls"）
# alpine/musl 下省去 OpenSSL 编译，构建更快、镜像更小
RUN cargo build --release --no-default-features

FROM alpine:3.21

RUN apk add --no-cache ca-certificates

WORKDIR /app
COPY --from=builder /app/target/release/kirostudio /app/kirostudio

VOLUME ["/app/config"]

EXPOSE 8990

CMD ["./kirostudio", "-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]
