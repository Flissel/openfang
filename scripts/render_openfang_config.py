#!/usr/bin/env python
"""Render openfang.vibemind.toml from its cross-platform template.

The template (openfang.vibemind.toml.template) is the git-tracked source of
truth. It carries ${VIBEMIND_ROOT} / ${HOME} placeholders instead of hardcoded
`C:\\Users\\User\\...` paths. This script substitutes the real paths for THIS
machine and writes openfang.vibemind.toml (which is gitignored — it is a
generated, machine-specific artifact).

Run this once after checkout, and again whenever the template changes:
    python vibemind-os/openfang/scripts/render_openfang_config.py

OpenFang's launcher (Vibemind.debug.ps1) starts the daemon with
`--config openfang.vibemind.toml`, so the rendered file is what actually
takes effect.

Placeholders:
    ${VIBEMIND_ROOT}  -> the Vibemind_V1 checkout root (vibemind_shared.paths)
    ${HOME}           -> the current user's home directory

Paths are emitted with forward slashes — valid in TOML on every OS, and what
OpenFang's Rust TOML loader expects.
"""

from __future__ import annotations

import sys
from pathlib import Path

# Make vibemind_shared importable without requiring an editable install:
# this script is at vibemind-os/openfang/scripts/, so the shared src is
# ../../shared/src relative to here.
_HERE = Path(__file__).resolve().parent
_SHARED_SRC = _HERE.parent.parent / "shared" / "src"
if _SHARED_SRC.is_dir():
    sys.path.insert(0, str(_SHARED_SRC))

try:
    from vibemind_shared.paths import repo_root
except ImportError:
    # Fallback: derive repo root the same way paths.py does — climb until a
    # vibemind-os/shared dir appears.
    def repo_root() -> Path:  # type: ignore
        for parent in _HERE.resolve().parents:
            if (parent / "vibemind-os" / "shared").is_dir():
                return parent
        return Path.cwd()


TEMPLATE = _HERE.parent / "openfang.vibemind.toml.template"
OUTPUT = _HERE.parent / "openfang.vibemind.toml"


def main() -> int:
    if not TEMPLATE.is_file():
        print(f"ERROR: template not found: {TEMPLATE}", file=sys.stderr)
        return 1

    root = repo_root()
    home = Path.home()

    # Forward slashes: valid in TOML on all platforms, and OpenFang's loader
    # expects them. as_posix() converts a Windows path to forward-slash form.
    root_str = Path(root).as_posix()
    home_str = Path(home).as_posix()

    text = TEMPLATE.read_text(encoding="utf-8")
    rendered = (
        text
        .replace("${VIBEMIND_ROOT}", root_str)
        .replace("${HOME}", home_str)
    )

    # Safety check — no placeholder should survive.
    leftover = [ln for ln in rendered.splitlines()
                if "${VIBEMIND_ROOT}" in ln or "${HOME}" in ln]
    if leftover:
        print(f"ERROR: {len(leftover)} placeholder(s) not substituted:",
              file=sys.stderr)
        for ln in leftover[:5]:
            print(f"  {ln}", file=sys.stderr)
        return 1

    OUTPUT.write_text(rendered, encoding="utf-8", newline="\n")
    print(f"rendered: {OUTPUT}")
    print(f"  VIBEMIND_ROOT -> {root_str}")
    print(f"  HOME          -> {home_str}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
