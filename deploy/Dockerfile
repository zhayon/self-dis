# Сборка linux-бинаря. Запускать так: scripts/build-linux.sh
# Финальный этап только вынимает файл, сам бинарь рассчитан на glibc Ubuntu.

FROM rust:1-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release -p self-dis-server --bin self-dis \
    && cp target/release/self-dis /tmp/self-dis

FROM scratch
COPY --from=build /tmp/self-dis /self-dis
