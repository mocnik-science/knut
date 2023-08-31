"""knut: compact OpenStreetMap history files with a space-time index.

*knut* is short for *knutpunkt*, the Swedish word for 'junction' or 'node', which is the
building blocks of OpenStreetMap data.

Query results can be converted with ``to_pylist()`` (list of dicts) or ``to_arrow()``
(``pyarrow.Table``, requires pyarrow).

Example::

    import knut
    db = knut.open("bremen.knut")
    t = db.snapshot((8.80, 53.07, 8.82, 53.08), "2023-01-01", key="highway").to_arrow()
"""

from ._knut import ArrowResult, Knut, __version__

__all__ = ["open", "Knut", "ArrowResult", "__version__"]


def open(path: str, verify_checksums: bool = True, cache_blocks: int = 1024) -> Knut:
    """Open an ``.knut`` file (memory mapped, read-only)."""
    return Knut(path, verify_checksums=verify_checksums, cache_blocks=cache_blocks)
