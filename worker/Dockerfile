# Build from the repo root:  docker build -f worker/Dockerfile .

# 1. The worker. Dependencies are built in their own layer so a code change
#    does not rebuild every crate.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY worker/Cargo.toml worker/Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY worker/src ./src
RUN touch src/main.rs && cargo build --release

# 2. whisper.cpp for speech to text, plus its small English model.
FROM debian:bookworm-slim AS whisper
RUN apt-get update && apt-get install -y --no-install-recommends build-essential cmake git ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
ARG WHISPER_TAG=v1.9.5
RUN git clone --depth 1 --branch ${WHISPER_TAG} https://github.com/ggml-org/whisper.cpp /w \
 && cmake -S /w -B /w/build -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON \
 && cmake --build /w/build -j"$(nproc)" --config Release --target whisper-cli
RUN curl -fsSL -o /ggml-base.en.bin https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin \
 && echo "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002  /ggml-base.en.bin" | sha256sum -c -

# 3. Runtime: ffmpeg from Debian has libass, which burns in the captions.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg ca-certificates fontconfig \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/streamcut-worker /usr/local/bin/
COPY --from=whisper /w/build/bin/whisper-cli /usr/local/bin/
COPY --from=whisper /ggml-base.en.bin /models/ggml-base.en.bin
COPY fonts /app/fonts
ENV WHISPER_MODEL=/models/ggml-base.en.bin \
    FONTS_DIR=/app/fonts \
    DATA_DIR=/data \
    ENCODER=libx264
RUN useradd -u 1000 -m app && mkdir -p /data && chown app /data
USER app
CMD ["streamcut-worker"]
