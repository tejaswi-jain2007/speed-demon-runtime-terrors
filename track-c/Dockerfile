FROM rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS build
WORKDIR /src
COPY . .
RUN rustc -C opt-level=3 -C target-cpu=native -C codegen-units=1 -C panic=abort -C lto=fat main.rs -o /src/solution

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
COPY --from=build /src/solution /app/solution
ENTRYPOINT ["/app/solution"]
