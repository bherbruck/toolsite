FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# cache deps separately from source changes
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=builder /app/target/release/page-host /usr/local/bin/page-host

ENV DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 8080

CMD ["page-host"]
