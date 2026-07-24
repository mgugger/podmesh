FROM rust:latest AS builder
ARG PODMAN_REMOTE_VERSION=5.6.2
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl musl-tools \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN set -eu; \
    case "$(uname -m)" in \
      x86_64) rust_target=x86_64-unknown-linux-musl; podman_arch=amd64 ;; \
      aarch64|arm64) rust_target=aarch64-unknown-linux-musl; podman_arch=arm64 ;; \
      *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    rustup target add "$rust_target"; \
    cargo build --release --locked -p podmesh-agent --target "$rust_target"; \
    install -Dm0755 "target/$rust_target/release/podmesh-agent" /out/podmesh-agent; \
    curl -fsSL "https://github.com/containers/podman/releases/download/v${PODMAN_REMOTE_VERSION}/podman-remote-static-linux_${podman_arch}.tar.gz" -o /tmp/podman.tar.gz; \
    tar -xzf /tmp/podman.tar.gz -C /tmp; \
    install -Dm0755 "/tmp/bin/podman-remote-static-linux_${podman_arch}" /out/podman

FROM scratch
ENV HOME=/var/lib/podmesh-agent
ENV PATH=/usr/local/bin
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /out/podmesh-agent /usr/local/bin/podmesh-agent
COPY --from=builder /out/podman /usr/local/bin/podman
ENTRYPOINT ["/usr/local/bin/podmesh-agent"]