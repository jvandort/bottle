#!/bin/sh
set -eu

# gcc only recommends libc6-dev, and without it every link fails ("ld returned
# 1 exit status"), so it's named. g++, make and build-essential aren't needed
# for cargo's build scripts, and aren't installed.
apt-get update
apt-get install -y --no-install-recommends gcc libc6-dev
rm -rf /var/lib/apt/lists/*
