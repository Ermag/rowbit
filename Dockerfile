# Rowbit's container image, ghcr.io/ermag/rowbit. Built by the release
# workflow from the static Linux binaries it has already built and checked,
# so the image holds exactly the binary the release ships. See "The
# container image" in docs/spec/setup.md.
#
#   docker run -i --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
#     --user "$(id -u):$(id -g)" -v ~/.rowbit/default:/data \
#     ghcr.io/ermag/rowbit mcp --agent claude-code

# Static: no libc needed; CA certificates for signing in through an
# identity provider; runs as a non-root user unless --user says otherwise.
# Pinned by digest; Dependabot proposes updates.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

# dist/amd64/rowbit and dist/arm64/rowbit, unpacked from the release's
# archives by the release workflow.
ARG TARGETARCH
COPY dist/${TARGETARCH}/rowbit /usr/local/bin/rowbit

# Where `rowbit setup` put Rowbit's files, mounted from the host: rowbit.yml,
# rowbit-server.yml, rowbit.db and the env file holding the database URL.
ENV ROWBIT_DIR=/data
WORKDIR /data
EXPOSE 8080

LABEL org.opencontainers.image.title="Rowbit" \
      org.opencontainers.image.description="A safety net for AI agents that write to your database" \
      org.opencontainers.image.source="https://github.com/Ermag/rowbit" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      io.modelcontextprotocol.server.name="io.github.Ermag/rowbit"

ENTRYPOINT ["/usr/local/bin/rowbit"]
CMD ["--help"]
