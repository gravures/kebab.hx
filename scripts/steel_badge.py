# /// script
# dependencies = ["genbadge==1.1.3"]
# ///

import sys
from shutil import which
from subprocess import run, SubprocessError
from pathlib import Path

from genbadge import Badge  # type: ignore[import-not-found]


def main() -> int:
    code = 0
    if (steel := which("steel")) is None:
        return 1

    try:
        cp = run([steel, "--version"], text=True, shell=False, capture_output=True, check=True)  # noqa: S603
    except SubprocessError as err:
        code = 1
        sys.stderr.write(str(err))
    else:
        file = str(Path(__file__).parent.parent / "assets" / "steel.svg")
        version = cp.stdout.strip().split(" ")[-1]
        badge = Badge(left_txt="Steel", right_txt=version, color="purple")
        badge.write_to(file, use_shields=False)
        sys.stderr.write(str(badge))
    return code


if __name__ == "__main__":
    sys.exit(main())
