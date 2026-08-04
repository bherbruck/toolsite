FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# Dependencies change far less often than source, so build them against a stub
# first and let that layer be reused.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Everything else, deliberately: the build reads more than src/ at compile
# time — wit/ through the component bindgen macro, migrations/ and templates/
# through include_str! — and listing those by hand has broken this twice.
# .dockerignore decides what stays out.
COPY . .
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

# A handler that reaches an API needs somewhere to check certificates
# against. Without this the HTTP client refuses to build at all — "No CA
# certificates were loaded from the system" — and every outbound call fails
# identically, before a packet moves.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/toolsite /usr/local/bin/toolsite

ENV TOOLSITE_DATA_DIR=/data
EXPOSE 8080

CMD ["toolsite"]
