ARG BUILD_FROM
# Must point at this fork's image, otherwise the add-on ships upstream's
# binary and none of the Bluetooth work is present.
FROM ghcr.io/phischl/govee2mqtt:latest AS govee2mqtt
FROM $BUILD_FROM

# Home Assistant's base image lags Debian's security updates -- it shipped
# util-linux 2.41-5 while 2.41.5-0+deb13u1 was out with four fixes. Pulling
# them here is the only lever we have over a base image we do not build.
# Checked: bashio and the s6 with-contenv shim survive the upgrade.
RUN apt-get update \
    && apt-get upgrade -y --no-install-recommends \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

COPY run.sh /run.sh
COPY --from=govee2mqtt /app/govee /app/
COPY --from=govee2mqtt /app/assets /app/assets/
COPY --from=govee2mqtt /app/AmazonRootCA1.pem /app/
CMD [ "/run.sh" ]
