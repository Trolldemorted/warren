FROM rust:1.96-bookworm AS builder
WORKDIR /build
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY warren/Cargo.toml warren/
COPY warren-cli/Cargo.toml warren-cli/
COPY rabbit/Cargo.toml rabbit/
RUN mkdir -p warren/src warren-cli/src \
 && echo 'fn main(){println!("fake main")}' > warren/src/main.rs \
 && echo 'fn main(){println!("fake main")}' > warren-cli/src/main.rs \
 && echo 'fn main(){println!("fake main")}' > rabiit/src/main.rs \
 && cargo build --release --bin warren --bin warren-cli --bin rabbit \
 && rm -rf warren/src warren-cli/src
COPY warren/src warren/src
COPY warren/migrations_atlas warren/migrations_atlas
COPY warren/templates warren/templates
COPY warren/openapi.yml warren/openapi.yml
COPY warren/static warren/static
COPY warren-cli/src warren-cli/src
COPY rabbit/src rabbit/src
RUN touch warren/src/main.rs warren-cli/src/main.rs && cargo build --release --bin warren --bin warren-cli --bin rabbit

FROM debian:bookworm-slim AS swagger
ARG SWAGGER_UI_VERSION=5.32.8
WORKDIR /tmp/swagger-ui
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && curl -fsSL "https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-${SWAGGER_UI_VERSION}.tgz" \
    -o swagger-ui.tgz \
 && mkdir -p extracted \
 && tar -xzf swagger-ui.tgz -C extracted \
 && cp extracted/package/swagger-ui.css . \
 && cp extracted/package/swagger-ui-bundle.js . \
 && cp extracted/package/swagger-ui-standalone-preset.js . \
 && cp extracted/package/index.css . \
 && cp extracted/package/favicon-16x16.png . \
 && cp extracted/package/favicon-32x32.png . \
 && cp extracted/package/oauth2-redirect.html . \
 && cp extracted/package/oauth2-redirect.js . \
 && cp extracted/package/LICENSE . \
 && cp extracted/package/NOTICE . \
 && rm -rf swagger-ui.tgz extracted \
 && rm -f swagger-ui.css.map swagger-ui-bundle.js.map swagger-ui-standalone-preset.js.map

FROM debian:bookworm-slim
ENV RUST_BACKTRACE=1
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 1000 --create-home --shell /usr/sbin/nologin warren
RUN curl -sSf https://atlasgo.sh | sh
COPY --from=builder /build/target/release/warren /usr/local/bin/warren
COPY --from=builder /build/target/release/warren-cli /usr/local/bin/warren-cli
COPY --from=builder /build/warren/static /var/lib/warren/static
COPY --from=builder /build/target/release/rabbit /usr/local/bin/rabbit
COPY --from=swagger /tmp/swagger-ui /var/lib/warren/docs
COPY warren/migrations_atlas warren/migrations_atlas
USER warren
ENV WARREN_STATIC_DIR=/var/lib/warren/static
ENV WARREN_DOCS_DIR=/var/lib/warren/docs
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/warren"]
