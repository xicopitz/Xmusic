FROM rust:1.98-slim-trixie AS build
WORKDIR /src
# static fpcalc (chromaprint) for AcoustID fingerprints; Debian's package drags in ~160 MB of ffmpeg libraries
# ponytail: x86_64 only; for arm64 pick the -linux-arm64 tarball and its checksum via TARGETARCH
ARG FPCALC=1.6.1
ARG FPCALC_SHA256=fc16cd37a70168040bc9ceb45f1d4d1216f5a75bc4c9cf8564bea70ac6a45733
ADD --checksum=sha256:${FPCALC_SHA256} https://github.com/acoustid/chromaprint/releases/download/v${FPCALC}/chromaprint-fpcalc-${FPCALC}-linux-x86_64.tar.gz /tmp/fpcalc.tgz
RUN tar xzf /tmp/fpcalc.tgz --strip-components=1 -C /usr/local/bin
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY assets assets
RUN cargo build --release --locked

# glibc + tzdata (the cron schedule follows TZ), no shell; TLS roots are built into the binary
FROM gcr.io/distroless/cc-debian13
COPY --from=build /usr/bin/nice /usr/bin/nice
COPY --from=build /usr/local/bin/fpcalc /usr/local/bin/fpcalc
COPY --from=build /src/target/release/xmusic /usr/local/bin/xmusic
ENV XMUSIC_DATA=/data XMUSIC_PORT=8337
EXPOSE 8337
CMD ["xmusic"]
