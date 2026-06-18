from __future__ import annotations


class BrowserUseError(Exception):
    """Base exception for the Python browser-use compatibility package."""


class BrowserUseProtocolError(BrowserUseError):
    """Raised when the Rust SDK server returns an invalid protocol response."""


class BrowserUseRuntimeError(BrowserUseError):
    """Raised when the Rust SDK runtime reports an execution error."""


class BrowserAlreadyInUseError(BrowserUseError):
    """Raised when two running agents try to share one Browser."""
