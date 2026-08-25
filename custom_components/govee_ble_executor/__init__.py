"""Govee BLE Executor.

A generic BLE executor driven over MQTT by the govee2mqtt add-on.

The add-on cannot reach Bluetooth itself: it runs in a container without a
Bluetooth stack, and an ESPHome proxy accepts only a single advertisement
subscriber, so it cannot speak to the proxies alongside Home Assistant. This
integration closes that gap by executing GATT operations through Home
Assistant's own Bluetooth stack.

It knows nothing about Govee. All protocol knowledge and all scheduling live in
the add-on; this side connects, writes bytes, and reports what came back.
"""

from __future__ import annotations

from homeassistant.components import mqtt
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryNotReady

from .const import (
    CONF_IDLE_TIMEOUT,
    CONF_MAX_CONCURRENT,
    CONF_TOPIC_PREFIX,
    DEFAULT_IDLE_TIMEOUT,
    DEFAULT_MAX_CONCURRENT,
    DEFAULT_TOPIC_PREFIX,
)
from .executor import BleExecutor

type GoveeBleExecutorEntry = ConfigEntry[BleExecutor]


async def async_setup_entry(hass: HomeAssistant, entry: GoveeBleExecutorEntry) -> bool:
    """Set up the executor from a config entry."""
    if not await mqtt.async_wait_for_mqtt_client(hass):
        raise ConfigEntryNotReady("The MQTT integration is not available yet")

    options = {**entry.data, **entry.options}
    executor = BleExecutor(
        hass,
        topic_prefix=options.get(CONF_TOPIC_PREFIX, DEFAULT_TOPIC_PREFIX),
        max_concurrent=options.get(CONF_MAX_CONCURRENT, DEFAULT_MAX_CONCURRENT),
        idle_timeout=options.get(CONF_IDLE_TIMEOUT, DEFAULT_IDLE_TIMEOUT),
    )
    await executor.async_start()

    entry.runtime_data = executor
    entry.async_on_unload(entry.add_update_listener(_async_reload_on_options_change))
    return True


async def async_unload_entry(hass: HomeAssistant, entry: GoveeBleExecutorEntry) -> bool:
    """Tear down the executor, closing any open connections."""
    await entry.runtime_data.async_stop()
    return True


async def _async_reload_on_options_change(
    hass: HomeAssistant, entry: GoveeBleExecutorEntry
) -> None:
    await hass.config_entries.async_reload(entry.entry_id)
