FROM rust:alpine AS builder
WORKDIR /usr/src/app
RUN apk add --no-cache musl-dev perl make gcc
COPY . .
RUN cargo build --release

FROM alpine:latest
WORKDIR /app
RUN apk add --no-cache tini
COPY --from=builder /usr/src/app/target/release/game-server .
ENTRYPOINT ["/sbin/tini", "--"]
CMD ["./game-server"]
