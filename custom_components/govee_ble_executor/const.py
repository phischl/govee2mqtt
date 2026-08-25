"""Constants for the Govee BLE Executor integration."""

from __future__ import annotations

DOMAIN = "govee_ble_executor"

CONF_TOPIC_PREFIX = "topic_prefix"
CONF_MAX_CONCURRENT = "max_concurrent"
CONF_IDLE_TIMEOUT = "idle_timeout"

DEFAULT_TOPIC_PREFIX = "gv2mqtt/ble"

# One session at a time by default. Home Assistant has no BLE queue of its own:
# parallel jobs really do produce parallel connects, and habluetooth's path
# scorer penalises every connect already in flight on a proxy while excluding a
# proxy whose slots are exhausted. Staying at one keeps us out of that regime
# and leaves the household's other BLE devices alone.
DEFAULT_MAX_CONCURRENT = 1

# Seconds a connection is kept open after the last operation. Long enough that a
# burst of commands reuses one connection, short enough that we are not sitting
# on one of a proxy's three slots.
DEFAULT_IDLE_TIMEOUT = 30.0

# Used when a request carries no deadline of its own. The add-on always sends
# one; this only guards against a hand-crafted request pinning a worker.
DEFAULT_JOB_BUDGET_MS = 30000

TOPIC_REQUEST = "req"
TOPIC_RESPONSE = "res"
TOPIC_STATUS = "status"

# Retry hint handed back for slot exhaustion. bleak-retry-connector backs off by
# this much internally, so asking the scheduler to retry sooner just burns
# attempts.
OUT_OF_SLOTS_RETRY_MS = 4000
