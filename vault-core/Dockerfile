FROM rust:1-slim-bookworm AS build
WORKDIR /src
# Dependency cache: build an empty main before the real code.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home vault \
    && mkdir -p /data/vault /data/state \
    && chown -R vault:vault /data
COPY --from=build /src/target/release/vault-core /usr/local/bin/vault-core
USER vault
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/vault-core"]
