FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:stable-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/claude-pool /usr/local/bin/claude-pool
WORKDIR /data
EXPOSE 8787
ENTRYPOINT ["claude-pool"]
CMD ["serve", "-c", "/data/claude-pool.toml"]
