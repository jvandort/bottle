#!/bin/sh
set -eu

# Its packages, gcc and libc6-dev, are its customizations.bottle.apt's,
# installed before this runs. gcc only recommends libc6-dev, and without it
# every link fails ("ld returned 1 exit status"), so it's named. g++, make and
# build-essential aren't needed for cargo's build scripts, and aren't installed.
