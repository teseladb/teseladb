# The public image of TeselaDB (viuzi/teseladb on Docker Hub), one per architecture, from the binary the release workflow
# cross-compiled. It runs nothing at build time, so the image of each architecture builds on any host without emulation,
# and `docker manifest` joins them under one tag. Debian slim keeps a shell and coreutils, which the guides use with
# `docker exec` (cat of root-credentials.txt). Build context: dist/image/<arch>/, with the binary, LICENSE, COMMERCIAL.md
# and an empty data/.
#   docker build --platform linux/arm64 --build-arg BASE=debian@sha256:<digest of arm64> -f docker/release.Dockerfile \
#     -t viuzi/teseladb:0.1.0-arm64 dist/image/arm64
# The base is debian:bookworm-slim pinned by the digest of the platform being built, which publish.sh passes as BASE:
# with the digest of the multi-platform index, the classic builder put the base layer of one platform into the image of
# the other, and Docker Hub refused the pull of viuzi/teseladb:0.1.0 on arm64 with «wrong diff id».
ARG BASE
FROM ${BASE}

COPY teseladb /usr/local/bin/teseladb
COPY LICENSE COMMERCIAL.md /usr/share/doc/teseladb/
# The node runs as an unprivileged user that owns /data, where a volume keeps the data, the PKI and the credentials.
COPY --chown=65532:65532 data /data
USER 65532:65532
VOLUME ["/data"]
# Clients, traffic between nodes, administration, REST API and MCP.
EXPOSE 27017 27100 27200 27080
ENTRYPOINT ["teseladb"]
CMD ["start", "--dbpath", "/data", "--bind_ip", "0.0.0.0"]
