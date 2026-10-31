# x86_64 user-mode emulator for scripts/test-x86_64.sh. `-cpu max` exposes SSE4.2, SSSE3, SSE4.1
# and the SHA extensions on any host, which Docker's own `linux/amd64` emulation does not. The
# system's bundle of certificate authorities is there, as on any Linux host: the outbound HTTPS
# and LDAP clients (013 FR-025) read it by default.
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends qemu-user ca-certificates \
    && rm -rf /var/lib/apt/lists/*
