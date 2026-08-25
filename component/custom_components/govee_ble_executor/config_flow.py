"""Config and options flow.

A single instance covers every device: the add-on addresses devices by MAC in
each job, so there is nothing per-device to configure here.
"""

from __future__ import annotations

from typing import Any

import voluptuous as vol
from homeassistant.config_entries import (
    ConfigEntry,
    ConfigFlow,
    ConfigFlowResult,
    OptionsFlow,
)
from homeassistant.core import callback
from homeassistant.helpers import selector

from .const import (
    CONF_IDLE_TIMEOUT,
    CONF_MAX_CONCURRENT,
    CONF_TOPIC_PREFIX,
    DEFAULT_IDLE_TIMEOUT,
    DEFAULT_MAX_CONCURRENT,
    DEFAULT_TOPIC_PREFIX,
    DOMAIN,
)


def _schema(defaults: dict[str, Any]) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(
                CONF_TOPIC_PREFIX,
                default=defaults.get(CONF_TOPIC_PREFIX, DEFAULT_TOPIC_PREFIX),
            ): str,
            vol.Required(
                CONF_MAX_CONCURRENT,
                default=defaults.get(CONF_MAX_CONCURRENT, DEFAULT_MAX_CONCURRENT),
            ): selector.NumberSelector(
                selector.NumberSelectorConfig(min=1, max=8, step=1, mode="box")
            ),
            vol.Required(
                CONF_IDLE_TIMEOUT,
                default=defaults.get(CONF_IDLE_TIMEOUT, DEFAULT_IDLE_TIMEOUT),
            ): selector.NumberSelector(
                selector.NumberSelectorConfig(
                    min=0, max=600, step=5, mode="box", unit_of_measurement="s"
                )
            ),
        }
    )


def _coerce(user_input: dict[str, Any]) -> dict[str, Any]:
    return {
        CONF_TOPIC_PREFIX: str(user_input[CONF_TOPIC_PREFIX]).rstrip("/"),
        CONF_MAX_CONCURRENT: int(user_input[CONF_MAX_CONCURRENT]),
        CONF_IDLE_TIMEOUT: float(user_input[CONF_IDLE_TIMEOUT]),
    }


class GoveeBleExecutorConfigFlow(ConfigFlow, domain=DOMAIN):
    """Handle initial setup."""

    VERSION = 1

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        self._async_abort_entries_match()

        if user_input is not None:
            return self.async_create_entry(title="Govee BLE Executor", data=_coerce(user_input))

        return self.async_show_form(step_id="user", data_schema=_schema({}))

    @staticmethod
    @callback
    def async_get_options_flow(entry: ConfigEntry) -> OptionsFlow:
        return GoveeBleExecutorOptionsFlow()


class GoveeBleExecutorOptionsFlow(OptionsFlow):
    """Allow the throttling settings to be adjusted after setup."""

    async def async_step_init(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        if user_input is not None:
            return self.async_create_entry(data=_coerce(user_input))

        defaults = {**self.config_entry.data, **self.config_entry.options}
        return self.async_show_form(step_id="init", data_schema=_schema(defaults))
