FROM rust:1.88-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release

FROM alpine:3.22
RUN adduser -D -H -u 10001 dufs
COPY --from=build /src/target/release/dufs /usr/local/bin/dufs
USER dufs
EXPOSE 5000
ENTRYPOINT ["dufs"]
