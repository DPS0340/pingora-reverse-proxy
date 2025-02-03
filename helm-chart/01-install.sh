#!/usr/bin/env bash

# Enable debug output
PS4="\n\033[1;33m>>\033[0m "; set -x

LOCATION=$(realpath "$0")
DIR=$(dirname "$LOCATION")

# Cache sudo credential
sudo -v

helm upgrade --install --create-namespace -n pingora-reverse-proxy pingora-reverse-proxy $DIR -f $DIR/values.yaml --wait
