####################################################################################################
## Builder
####################################################################################################
# Pinned rather than :latest so a build is reproducible and Dependabot can
# see when it moves. This stage only exists to mint /etc/passwd and an
# empty /data for the final image.
FROM --platform=$BUILDPLATFORM alpine:3.24 AS builder
ARG TARGETPLATFORM

RUN adduser \
    --disabled-password \
    --gecos "" \
    --home "/nonexistent" \
    --shell "/sbin/nologin" \
    --no-create-home \
    --uid "1000" \
    "govee"

WORKDIR /work
COPY docker-target/$TARGETPLATFORM/govee /work

# Creates an empty /data dir that we can use to copy and chown in the next stage
WORKDIR /data

####################################################################################################
## Final image
####################################################################################################
# `static`, not `cc`: the binary is a static-pie musl build and never calls
# into the base image's libc. Carrying glibc anyway added nothing but its
# CVEs -- a dozen of them in the first Trivy scan. Verified against the
# published binary: it runs here and completes a TLS handshake to Govee,
# because distroless/static ships ca-certificates too.
FROM gcr.io/distroless/static-debian13@sha256:0985f124d25d79a432b79e806764a9deb759e5c664be7c0633b9f13c3e12cbc0

# Import from builder.
COPY --from=builder /etc/passwd /etc/passwd
COPY --from=builder /etc/group /etc/group
#COPY --from=builder /etc/ssl/certs /etc/ssl/certs

WORKDIR /app

COPY --from=builder /work/govee /app/govee
COPY AmazonRootCA1.pem /app
COPY --from=builder --chown=govee:govee /data /data
COPY assets /app/assets

USER govee:govee
LABEL org.opencontainers.image.source="https://github.com/phischl/govee2mqtt"
ENV \
  RUST_BACKTRACE=full \
  PATH=/app:$PATH \
  XDG_CACHE_HOME=/data

VOLUME /data

CMD ["/app/govee", \
  "serve", \
  "--govee-iot-key=/data/iot.key", \
  "--govee-iot-cert=/data/iot.cert", \
  "--amazon-root-ca=/app/AmazonRootCA1.pem"]


