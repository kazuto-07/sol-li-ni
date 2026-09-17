# syntax=docker/dockerfile:1

# ---- build ---------------------------------------------------------------------------------
FROM rust:1-slim-bookworm AS build
WORKDIR /app

# `ring` compiles C, and the slim image has no compiler.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*

# Dependencies first, so source edits do not invalidate the (slow) dependency layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/sol_li_ni* target/release/sol-li-ni

COPY src ./src
COPY web ./web
RUN cargo build --release --locked --bin sol-li-ni

# ---- run -----------------------------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin solli

COPY --from=build /app/target/release/sol-li-ni /usr/local/bin/sol-li-ni
USER solli

# Listen on all interfaces inside the container; media uses a fixed UDP range so it can be
# published and firewalled. Set PUBLIC_IP to the host's public address when deploying.
ENV ADDR=0.0.0.0:8080 \
    RTC_PORT_MIN=40000 \
    RTC_PORT_MAX=40099
EXPOSE 8080/tcp 40000-40099/udp

ENTRYPOINT ["/usr/local/bin/sol-li-ni"]
