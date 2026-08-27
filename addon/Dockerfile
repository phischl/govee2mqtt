ARG BUILD_FROM
# Must point at this fork's image, otherwise the add-on ships upstream's
# binary and none of the Bluetooth work is present.
#
# Pinned to BUILD_VERSION rather than :latest so a tagged add-on is
# reproducible. The builder reads BUILD_VERSION from addon/config.yaml, where
# scripts/apply-tag.sh has stamped the git tag, and Container Build publishes
# the standalone image under exactly that tag from the same commit. With
# :latest the add-on contained whatever main had built most recently, so
# rebuilding an old tag produced a different image.
#
# The addon job needs the merge job, so the tag is always published before this
# runs. A version that was never published fails the build with "manifest
# unknown" -- which is the honest outcome; the alternative is silently shipping
# a binary from a different commit.
ARG BUILD_VERSION
FROM ghcr.io/phischl/govee2mqtt:${BUILD_VERSION} AS govee2mqtt
FROM $BUILD_FROM

# Home Assistant's base image lags Debian's security updates -- it shipped
# util-linux 2.41-5 while 2.41.5-0+deb13u1 was out with four fixes. Pulling
# them here is the only lever we have over a base image we do not build.
# Checked: bashio and the s6 with-contenv shim survive the upgrade.
RUN apt-get update \
    && apt-get upgrade -y --no-install-recommends \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Home Assistant's base image ships tempio, a Go binary for rendering config
# templates from add-on options. Ours renders nothing -- run.sh reads its
# settings from the environment through bashio -- and nothing else in the image
# references it: /etc/services.d is empty and no init script mentions it.
#
# It is worth removing rather than tolerating. Being a static Go binary, it
# carries its own copies of golang.org/x/crypto and the Go standard library,
# and those account for every fixable critical or high finding Trivy reports
# against this image -- nineteen of them, none of which we can patch, because
# we do not build it. The Debian packages alongside have no fix available at
# all, which is why the apt-get upgrade above is the only other lever.
RUN rm -f /usr/bin/tempio

COPY run.sh /run.sh
COPY --from=govee2mqtt /app/govee /app/
COPY --from=govee2mqtt /app/assets /app/assets/
COPY --from=govee2mqtt /app/AmazonRootCA1.pem /app/
CMD [ "/run.sh" ]
