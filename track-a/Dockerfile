FROM halloumee/speed-demon-rust:1.99.1-trixie-llvm-tools@sha256:13e4ee7135d8eac6c56476c7d1aee201f36479f4745fd5d0e6b4529f4e13251a AS build
WORKDIR /src
COPY . .
RUN mkdir -p /tmp/prof && RUSTFLAGS="-Cprofile-generate=/tmp/prof" cargo build --release --offline --target-dir /tmp/inst && /tmp/inst/release/solution pgo/train.log /tmp/train-out.txt
RUN "$(rustc --print sysroot)"/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata merge -o /tmp/merged.profdata /tmp/prof
RUN RUSTFLAGS="-Cprofile-use=/tmp/merged.profdata" cargo build --release --offline

# Runtime stage: small allowlisted base, pinned by digest.
FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
COPY --from=build /src/target/release/solution /app/solution
# Reads /data/requests.log, writes /out/summary.txt, exits 0.
ENTRYPOINT ["/app/solution"]