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
# The proprietary licence. The image is built and run by its operator, never
# published; THIRD-PARTY-NOTICES joins it before that changes (docs/backlog.md).
COPY LICENSE /usr/share/doc/passalong-server/LICENSE
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
