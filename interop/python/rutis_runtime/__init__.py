"""The Python runtime of rutis-interop, and the SDK for Python plugins.

See `plugin` for how a plugin is written. rutis starts this package as
`python3 -m rutis_runtime <socket> <project>`.
"""

from .peer import RemoteError, SyncWaitCycle
from .plugin import Context, Plugin, define_plugin

__all__ = ["Context", "Plugin", "RemoteError", "SyncWaitCycle", "define_plugin"]
