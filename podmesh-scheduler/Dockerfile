FROM rust:latest AS builder
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates musl-tools \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN set -eu; \
    case "$(uname -m)" in \
      x86_64) rust_target=x86_64-unknown-linux-musl ;; \
      aarch64|arm64) rust_target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    rustup target add "$rust_target"; \
    cargo build --release --locked -p podmesh-scheduler --target "$rust_target"; \
    install -Dm0755 "target/$rust_target/release/podmesh-scheduler" /out/podmesh-scheduler

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /out/podmesh-scheduler /usr/local/bin/podmesh-scheduler
ENTRYPOINT ["/usr/local/bin/podmesh-scheduler"]