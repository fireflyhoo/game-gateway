# ---------- 构建阶段 ----------
FROM rust:1.83-slim-bookworm AS builder

WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# 用 release 配置编译，产出单文件二进制
RUN cargo build --release --locked 2>/dev/null || cargo build --release

# ---------- 运行阶段 ----------
FROM debian:bookworm-slim

WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/game-gateway /app/game-gateway

ENV HOST=0.0.0.0 \
    PORT=8080 \
    RUST_LOG=info

EXPOSE 8080

# 健康检查依赖 curl；用自带 shell + /dev/tcp 也可，这里镜像内装 curl 更稳妥
RUN apt-get update && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:${PORT}/health || exit 1

CMD ["/app/game-gateway"]
