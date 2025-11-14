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

RUN cargo build --release --locked -p workplane-gateway --target x86_64-unknown-linux-musl
RUN strip /app/target/x86_64-unknown-linux-musl/release/workplane-gateway

# =====================
# 2. Minimal runtime image
# =====================
FROM scratch

# Provide a minimal BusyBox for basic tooling (optional)
COPY --from=busybox:musl /bin/busybox /bin/busybox
RUN ["/bin/busybox", "ln", "-s", "/bin/busybox", "/bin/sh"]

# Basic /etc data needed by glibc-compatible binaries
COPY --from=alpine:latest /etc/passwd /etc/passwd
COPY --from=alpine:latest /etc/group /etc/group
COPY --from=alpine:latest /etc/hosts /etc/hosts
COPY --from=alpine:latest /etc/nsswitch.conf /etc/nsswitch.conf
COPY --from=alpine:latest /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Gateway binary
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/workplane-gateway /usr/local/bin/workplane-gateway

ENV PATH=/usr/local/bin:/bin

ENTRYPOINT ["workplane-gateway"]
