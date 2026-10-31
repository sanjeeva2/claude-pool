FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:stable-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/claude-pool /src/target/release/claude-sync /usr/local/bin/
WORKDIR /data
EXPOSE 8787 8790
# Default: the claude-sync hub. For the API gateway: `claude-pool serve -c /data/claude-pool.toml`.
CMD ["claude-sync", "hub", "-c", "/data/claude-sync-hub.toml"]
