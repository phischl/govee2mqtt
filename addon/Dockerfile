ARG BUILD_FROM
# Must point at this fork's image, otherwise the add-on ships upstream's
# binary and none of the Bluetooth work is present.
FROM ghcr.io/phischl/govee2mqtt:latest AS govee2mqtt
FROM $BUILD_FROM
COPY run.sh /run.sh
COPY --from=govee2mqtt /app/govee /app/
COPY --from=govee2mqtt /app/assets /app/assets/
COPY --from=govee2mqtt /app/AmazonRootCA1.pem /app/
CMD [ "/run.sh" ]
