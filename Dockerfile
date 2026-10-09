FROM rust:1.99-trixie@sha256:3745c050d12adc738eff16ebfc81ed044bfb2cc27c6828850ff1666beb1c7a49 AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests

FROM builder AS test
RUN cargo test --locked

FROM test AS release
RUN cargo build --release --locked

FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=release /app/target/release/lrc-sync /usr/local/bin/lrc-sync

CMD ["lrc-sync"]
