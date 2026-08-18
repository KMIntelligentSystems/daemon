# syntax=docker/dockerfile:1
# One image, two runtimes: Rust airlock + the Node oracle it spawns.
# Build context is c:/repos/daemon (the dir containing daemon/).
#   docker build -t daemon-airlock:1.0.0 .
#   — or, without a local Docker daemon —
#   az acr build -r <acr> -t daemon-airlock:1.0.0 .

# --- oracle build ---
FROM node:22-bookworm AS oracle
WORKDIR /src
COPY daemon/oracle/package*.json ./
RUN npm ci
COPY daemon/oracle/ ./
RUN npm run build                      # -> dist/main.js

# --- airlock build ---
FROM rust:1-bookworm AS airlock
WORKDIR /src
COPY daemon/airlock/Cargo.* ./
COPY daemon/airlock/src ./src
RUN cargo build --release              # libsqlite3-sys bundled → needs the C toolchain (present here)

# --- runtime ---
FROM node:22-bookworm-slim
WORKDIR /app
COPY --from=airlock /src/target/release/daemon-airlock /usr/local/bin/
COPY --from=oracle  /src/dist ./oracle/dist
COPY --from=oracle  /src/node_modules ./oracle/node_modules
COPY daemon/airlock/config.toml ./config.toml
COPY job-entry.sh /usr/local/bin/job-entry.sh
RUN sed -i 's/\r$//' /usr/local/bin/job-entry.sh && chmod +x /usr/local/bin/job-entry.sh
ENV PORT=8791
EXPOSE 8791
# NOTE: --config/--db are top-level clap args → they precede the subcommand.
# No --schedule: scheduling lives in ACA Jobs (design/deploy-rust-airlock.md §4).
CMD ["daemon-airlock", "--config", "/app/config.toml", "--db", "/data/sandbox.db", \
     "serve-http", "--port", "8791"]
