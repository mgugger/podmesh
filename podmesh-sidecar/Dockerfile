# =====================
# 1. Build the gateway binary
# =====================
FROM rust:latest AS builder

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

COPY . .

RUN cargo build --release --locked -p podmesh-sidecar --target x86_64-unknown-linux-musl

# =====================
# 2. Minimal runtime image
# =====================
FROM scratch

# Provide a minimal BusyBox for basic tooling (optional)
#COPY --from=busybox:musl /bin/busybox /bin/busybox
#RUN ["/bin/busybox", "ln", "-s", "/bin/busybox", "/bin/sh"]

# Basic /etc data needed by glibc-compatible binaries
COPY --from=alpine:latest /etc/passwd /etc/passwd
COPY --from=alpine:latest /etc/group /etc/group
COPY --from=alpine:latest /etc/hosts /etc/hosts
COPY --from=alpine:latest /etc/nsswitch.conf /etc/nsswitch.conf
COPY --from=alpine:latest /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Sidecar binary
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/podmesh-sidecar /usr/local/bin/podmesh-sidecar

ENV PATH=/usr/local/bin:/bin

ENTRYPOINT ["podmesh-sidecar"]

