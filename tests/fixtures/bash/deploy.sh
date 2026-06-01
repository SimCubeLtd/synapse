#!/usr/bin/env bash
set -euo pipefail

log() {
  echo "[deploy] $*"
}

build_image() {
  local tag="$1"
  docker build -t "synapse:${tag}" .
}

deploy_all() {
  log "starting"
  build_image latest
  log "done"
}

deploy_all
