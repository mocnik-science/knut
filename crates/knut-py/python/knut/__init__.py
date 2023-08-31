"""knut: compact OpenStreetMap history files with a space-time index.

*knut* is short for *knutpunkt*, the Swedish word for 'junction' or 'node', which is the
building blocks of OpenStreetMap data.

Query results can be converted with ``to_pylist()`` (list of dicts) or ``to_arrow()``
(``pyarrow.Table``, requires pyarrow).

Example::

    import knut
    db = knut.open("bremen.knutdb")
    t = db.snapshot((8.80, 53.07, 8.82, 53.08), "2023-01-01", key="highway").to_arrow()

Scripts in the knut language run with :func:`run`::

    outputs = knut.run("count of nodes n with n:amenity=bench", db="bremen.knutdb")
    outputs[0]["value"]   # 4089
"""

import builtins as _builtins
import json as _json
import os as _os
import warnings as _warnings

from ._knut import ArrowResult, Knut, __version__, run_script as _run_script

__all__ = ["open", "run", "run_file", "Knut", "ArrowResult", "KnutScriptError", "__version__"]


class KnutScriptError(Exception):
    """A script could not be run (exit code 1) or stopped while running (exit code 2).

    ``message`` holds the full text of all problems, as printed by ``knut script.knut``;
    ``outputs`` holds the outputs written before a stop.
    """

    def __init__(self, code: int, message: str, outputs=None):
        super().__init__(message)
        self.code = code
        self.message = message
        self.outputs = outputs or []


def open(path: str, verify_checksums: bool = True, cache_blocks: int = 1024) -> Knut:
    """Open a ``.knutdb`` file (memory mapped, read-only)."""
    return Knut(path, verify_checksums=verify_checksums, cache_blocks=cache_blocks)


def run(script: str, db=None, base_dir: str = ".", file=None):
    """Run a knut script and return its outputs.

    Each output is a dict with ``line``, ``expr``, ``seconds`` and ``value``. Numbers with a
    unit are ``{"value": …, "unit": …}``; groups are lists of dicts. ``db`` (a path or an
    opened :class:`Knut`) overrides ``knut_db`` in the script. Warnings are reported through
    the :mod:`warnings` module; errors raise :class:`KnutScriptError`.
    """
    if isinstance(db, Knut):
        db = db.path
    if db is not None:
        db = _os.fspath(db)
    code, out, err = _run_script(script, db, _os.fspath(base_dir), file)
    outputs = _json.loads(out) if out.strip() else []
    if code != 0:
        raise KnutScriptError(code, err, outputs)
    if err.strip():
        _warnings.warn(err, stacklevel=2)
    return outputs


def run_file(path, db=None):
    """Run a ``.knut`` file; ``knut_db`` is relative to the file's directory."""
    path = _os.fspath(path)
    with _builtins.open(path, encoding="utf-8") as f:
        script = f.read()
    return run(script, db=db, base_dir=_os.path.dirname(_os.path.abspath(path)), file=_os.path.basename(path))
