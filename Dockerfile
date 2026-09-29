FROM rust:1-slim AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-chef --locked

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked \
    && mkdir -p /image-data \
    && touch /image-data/.keep

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder --chown=65532:65532 /app/target/release/zenith-backend ./zenith-backend
COPY --from=builder --chown=65532:65532 /image-data /data

ENV DATABASE_URL=sqlite:///data/zenith.db
VOLUME ["/data"]
EXPOSE 8081
USER 65532:65532
ENTRYPOINT ["/app/zenith-backend"]
