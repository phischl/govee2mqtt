"""Make `protocol` importable without a Home Assistant checkout.

`protocol.py` deliberately has no Home Assistant imports so that the wire format
can be tested on its own. Importing it as `govee_ble_executor.protocol` would
still execute the package's `__init__.py`, which does import Home Assistant, so
the module directory itself goes on the path and the module is imported bare.
The rest of the package is exercised in a running Home Assistant instead.
"""

import sys
from pathlib import Path

PACKAGE = Path(__file__).resolve().parents[1] / "custom_components" / "govee_ble_executor"
sys.path.insert(0, str(PACKAGE))
