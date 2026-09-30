"""Native Rust DRF engine. Activate before importing DRF engine modules.

This development build has not yet passed the full compatibility matrix.
"""
from . import _native
from ._native import COMPATIBILITY_CERTIFIED, activate

_feasibility = _native._feasibility
_feasibility.__file__ = _native.__file__

__all__ = ['activate', 'COMPATIBILITY_CERTIFIED']
