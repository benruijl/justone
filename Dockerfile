# Self-hosted Just One: docker build -t justone . && docker run -p 3030:3030 justone
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY include include
COPY public public
RUN cargo build --release --locked --bin justone

FROM scratch
COPY --from=build /src/target/release/justone /justone
ENV PORT=3030
EXPOSE 3030
ENTRYPOINT ["/justone"]
