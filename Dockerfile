# syntax=docker/dockerfile:1

# Fonts: the same pinned set as `bash tools/fonts/fetch.sh`, checked against tools/fonts/SHA256SUMS.
FROM debian:bookworm-slim AS fonts
COPY tools/fonts/ /src/
# The Debian copyright files of the font packages ship with the image (GPL/OFL notices).
RUN bash /src/install.sh /fonts \
 && mkdir /licenses \
 && for d in /usr/share/doc/fonts-*/; do cp "$d/copyright" "/licenses/$(basename "$d").copyright"; done

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY crates/ crates/
COPY static/ static/
# target/ is a cache mount, so the binary is copied out in the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p mermaid-ink \
 && cp target/release/mermaid-ink /mermaid-ink

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=fonts /fonts /usr/share/mermaid-ink/fonts
COPY --from=build /mermaid-ink /usr/local/bin/mermaid-ink
COPY --from=fonts /licenses /usr/share/doc/mermaid-ink/fonts
COPY LICENSE THIRD_PARTY_NOTICES /usr/share/doc/mermaid-ink/
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/mermaid-ink"]
