#!/bin/bash
set -euo pipefail

cargo build -p sada-client --target i686-unknown-linux-gnu
DreamMaker dmsrc/sada.dme && DreamDaemon dmsrc/sada.dmb -trusted -port 5555
