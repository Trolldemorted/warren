FROM rust:1.96-bookworm AS builder
WORKDIR /build
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY warren/Cargo.toml warren/
COPY warren-cli/Cargo.toml warren-cli/
COPY rabbit/Cargo.toml rabbit/
COPY rabbit-lib/Cargo.toml rabbit-lib/
RUN mkdir -p warren/src warren-cli/src rabbit/src rabbit/src/bin rabbit-lib/src \
 && echo 'fn main(){println!("fake main")}' > warren/src/main.rs \
 && echo 'fn main(){println!("fake main")}' > warren-cli/src/main.rs \
 && echo 'fn main(){println!("fake main")}' > rabbit/src/main.rs \
 && echo 'fn main(){println!("fake rabbit-hook")}' > rabbit/src/bin/rabbit-hook.rs \
 # `rabbit` and `rabbit-lib` are both library crates (consumed by warren
 # and each other). The cache pre-warming layer must satisfy BOTH lib
 # targets, otherwise cargo refuses the dependency as "missing a lib
 # target" and downstream crates fail to resolve.
 && echo '// fake lib.rs — real source copied below' > rabbit/src/lib.rs \
 && echo '// fake lib.rs — real source copied below' > rabbit-lib/src/lib.rs \
 && cargo build --release --bin warren --bin warren-cli --bin rabbit --bin rabbit-hook \
 && rm -rf warren/src warren-cli/src rabbit/src rabbit-lib/src
COPY warren/src warren/src
COPY warren/migrations_atlas warren/migrations_atlas
COPY warren/templates warren/templates
COPY warren/openapi.yml warren/openapi.yml
COPY warren/static warren/static
COPY warren-cli/src warren-cli/src
COPY rabbit/src rabbit/src
COPY rabbit-lib/src rabbit-lib/src
# Force cargo to rebuild now that the real sources are in place. The
# `rabbit-lib` library target + the `rabbit` and `rabbit-hook` bin
# entry points need touching — without this, `cargo build` may reuse
# the cached fakes from the warming step and the resulting binaries
# won't reflect the real source.
RUN touch warren/src/main.rs \
         warren-cli/src/main.rs \
         rabbit/src/main.rs \
         rabbit/src/lib.rs \
         rabbit/src/bin/rabbit-hook.rs \
         rabbit-lib/src/lib.rs \
 && cargo build --release --bin warren --bin warren-cli --bin rabbit --bin rabbit-hook

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
COPY --from=builder /build/target/release/rabbit-hook /usr/local/bin/rabbit-hook
COPY --from=swagger /tmp/swagger-ui /var/lib/warren/docs
COPY warren/migrations_atlas warren/migrations_atlas
USER warren
ENV WARREN_STATIC_DIR=/var/lib/warren/static
ENV WARREN_DOCS_DIR=/var/lib/warren/docs
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/warren"]
