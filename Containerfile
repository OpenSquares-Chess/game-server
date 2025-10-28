FROM rust:latest AS builder
WORKDIR /usr/src/app
RUN rustup target add x86_64-unknown-linux-musl
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM alpine:latest
WORKDIR /app
RUN apk add --no-cache tini
COPY --from=builder /usr/src/app/target/x86_64-unknown-linux-musl/release/game-server .
ENTRYPOINT ["/sbin/tini", "--"]
CMD ["./game-server"]
