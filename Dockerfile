FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# Dependencies change far less often than source, so build them against a stub
# first and let that layer be reused.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Both are read at compile time, not run time: wit/ by the component bindgen
# macro, migrations/ by include_str!. Leaving either out fails the build.
COPY wit ./wit
COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=builder /app/target/release/toolsite /usr/local/bin/toolsite

ENV DATA_DIR=/data
EXPOSE 8080

CMD ["toolsite"]
