FROM rust:slim

WORKDIR /usr/src/justone
COPY . .

RUN cargo install --path .

CMD ["justone"]