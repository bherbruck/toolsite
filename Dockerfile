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

# A browser for screenshots, fetched in its own stage so curl and unzip stay
# out of the runtime image. Google's chrome-headless-shell is Chrome built for
# headless use only: no GTK, a few hundred MB instead of a full desktop
# browser. Skipped entirely unless WITH_BROWSER=1.
FROM debian:bookworm-slim AS browser
ARG WITH_BROWSER=0
ARG HEADLESS_SHELL_VERSION=154.0.8037.92
RUN mkdir -p /opt/browser \
    && if [ "$WITH_BROWSER" = "1" ]; then \
         apt-get update \
         && apt-get install -y --no-install-recommends ca-certificates curl unzip \
         && curl -fsSL -o /tmp/shell.zip \
              "https://storage.googleapis.com/chrome-for-testing-public/${HEADLESS_SHELL_VERSION}/linux64/chrome-headless-shell-linux64.zip" \
         && unzip -q /tmp/shell.zip -d /opt/browser \
         && rm /tmp/shell.zip; \
       fi

FROM debian:bookworm-slim
WORKDIR /app

# Screenshots are off unless asked for. Two ways to turn them on:
#   - build with WITH_BROWSER=1, and the image carries chrome-headless-shell;
#   - or leave this off and run a browser sidecar, named at run time by
#     TOOLSITE_BROWSER_URL (see README, "Looking at what you built").
# Railway passes a service variable called WITH_BROWSER into the build.
ARG WITH_BROWSER=0

# A handler that reaches an API needs somewhere to check certificates
# against. Without this the HTTP client refuses to build at all, "No CA
# certificates were loaded from the system", and every outbound call fails
# identically, before a packet moves.
#
# With the browser: the shared libraries chrome-headless-shell links against
# on bookworm (found with ldd), and fonts so text renders as text.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && if [ "$WITH_BROWSER" = "1" ]; then \
         apt-get install -y --no-install-recommends \
           libglib2.0-0 libnspr4 libnss3 libatk1.0-0 libatk-bridge2.0-0 \
           libatspi2.0-0 libdbus-1-3 libx11-6 libxcb1 libxext6 libxfixes3 \
           libxcomposite1 libxdamage1 libxrandr2 libxkbcommon0 libgbm1 \
           libexpat1 libasound2 fonts-liberation fonts-noto-color-emoji; \
       fi \
    && rm -rf /var/lib/apt/lists/*
COPY --from=browser /opt/browser /opt/browser
# Read only when the file exists, so an image without the browser is fine.
ENV TOOLSITE_BROWSER=/opt/browser/chrome-headless-shell-linux64/chrome-headless-shell
COPY --from=builder /app/target/release/toolsite /usr/local/bin/toolsite

ENV TOOLSITE_DATA_DIR=/data
EXPOSE 8080

CMD ["toolsite"]
