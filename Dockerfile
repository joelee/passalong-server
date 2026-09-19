# syntax=docker/dockerfile:1

# Builder and runtime share Debian trixie so the binary's glibc matches.
FROM rust:1.98.1-slim-trixie AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# The core embeds the sample: it is the file `init` writes.
COPY config.sample.toml ./
COPY crates ./crates
RUN cargo build --release --locked -p passalong-server

FROM debian:trixie-slim
RUN useradd --system --uid 10001 --home-dir /var/lib/passalong-server --create-home passalong-server \
    && mkdir -p /etc/passalong-server \
    && chmod 0700 /etc/passalong-server /var/lib/passalong-server \
    && chown passalong-server: /etc/passalong-server /var/lib/passalong-server
COPY --from=builder /src/target/release/passalong-server /usr/local/bin/passalong-server
# What a registry and `docker inspect` show. The release workflow passes the
# version and the commit; a local build has neither.
ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="passalong-server" \
      org.opencontainers.image.description="Self-hosted HTTPS server for passalong: clipboard and file sharing between your devices" \
      org.opencontainers.image.source="https://github.com/joelee/passalong-server" \
      org.opencontainers.image.url="https://github.com/joelee/passalong-server" \
      org.opencontainers.image.documentation="https://github.com/joelee/passalong-server/blob/main/deploy/docker/README.md" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
# The terms travel with the binary: AGPL-3.0-or-later.
COPY LICENSE THIRD-PARTY-NOTICES /usr/share/doc/passalong-server/
# Where `init` writes and every command looks: the configuration volume. A
# fresh named volume takes this directory's owner, which is the server's user.
ENV PASSALONG_SERVER_CONFIG_FILE=/etc/passalong-server/config.toml
USER passalong-server
WORKDIR /var/lib/passalong-server
VOLUME ["/var/lib/passalong-server", "/etc/passalong-server"]
EXPOSE 8443
# The image holds no curl: the binary asks the running server itself.
# While starting it is asked every other second, so that `healthy` follows
# readiness at once and not half a minute later.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --start-interval=2s \
    CMD ["passalong-server", "check", "--health"]
ENTRYPOINT ["passalong-server"]
CMD ["serve"]
