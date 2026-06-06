"""Module entry point: ``python -m aberp_cad_extract <input.stl> ...``."""

import sys

from aberp_cad_extract.cli import main

if __name__ == "__main__":
    sys.exit(main())
