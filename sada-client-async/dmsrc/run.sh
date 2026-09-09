#!/bin/bash
set -euo pipefail

cargo build -p sada-client-async --target i686-unknown-linux-gnu
DreamMaker sada-client-async/dmsrc/async.dme && DreamDaemon sada-client-async/dmsrc/async.dmb -trusted
