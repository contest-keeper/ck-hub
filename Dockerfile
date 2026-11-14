# syntax=docker/dockerfile:1
# Keeper Hub as a small static image. Build from the repository root: docker build -t ck-hub .

FROM rust:1.99.1-alpine AS build
RUN apk add --no-cache musl-dev
# The image already holds this exact toolchain, so the pin file is not needed (and would only
# try to download extra components).
ENV RUSTUP_TOOLCHAIN=1.99.1
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ck-hub \
 && cp target/release/ck-hub /ck-hub

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /ck-hub /ck-hub
# Listen on every interface inside the container. Put HTTPS in front of it: the session cookie is
# Secure. Give the database in CK_DATABASE_URL; without it the Hub forgets everything on restart.
ENV CK_LISTEN=0.0.0.0:8088
EXPOSE 8088
USER nonroot
ENTRYPOINT ["/ck-hub"]
