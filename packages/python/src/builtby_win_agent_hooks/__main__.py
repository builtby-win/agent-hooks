import os
import subprocess
import sys

from . import binary_path


def main() -> None:
    path = binary_path()
    if sys.platform == "win32":
        sys.exit(subprocess.call([path, *sys.argv[1:]]))
    os.execv(path, [path, *sys.argv[1:]])


if __name__ == "__main__":
    main()
