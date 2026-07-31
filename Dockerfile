FROM rust:1.88-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --create-home acp-tunnel
COPY --from=builder /build/target/release/acp-tunnel /usr/local/bin/acp-tunnel
USER acp-tunnel
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/acp-tunnel"]
CMD ["serve", "--config", "/etc/acp-tunnel/config.toml"]
