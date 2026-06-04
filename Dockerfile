# syntax=docker/dockerfile:1
#
# Minimal runtime image built from a PREBUILT static musl binary.
# Nothing is compiled here: BuildKit sets TARGETARCH (amd64 / arm64) for each
# requested platform and we copy the matching binary the CI `build` job already
# produced under binaries/<arch>/next-socks5. This means multi-arch images need
# no QEMU emulation — COPY just moves files.
FROM scratch

ARG TARGETARCH
COPY binaries/${TARGETARCH}/next-socks5 /usr/local/bin/next-socks5
COPY config.example.toml /etc/next-socks5/config.example.toml

# SOCKS5 (CONNECT + UDP ASSOCIATE) defaults to port 1080.
EXPOSE 1080/tcp 1080/udp

# Run as an unprivileged user (scratch has no /etc/passwd; a numeric UID works).
USER 65534:65534

# The container has no TTY, so always run headless. The entrypoint pins the
# `serve` subcommand and `--no-tui`; the CMD holds the default listen address and
# can be overridden at `docker run` time (those args append after `serve`).
ENTRYPOINT ["/usr/local/bin/next-socks5", "serve", "--no-tui"]
CMD ["--listen", "0.0.0.0:1080"]
