#!/usr/bin/env bash
set -euo pipefail

sha256_file() {
  sha256sum "$1" | cut -d' ' -f1
}

extract_manifest() {
  local archive=$1
  local destination=$2
  local listing=$3
  tar -tf "$archive" >"$listing"
  if grep -Eq '(^/|(^|/)\.\.(/|$))' "$listing"; then
    echo "image archive contains an unsafe path" >&2
    exit 1
  fi
  if [[ $(grep -Fxc manifest.json "$listing") -ne 1 ]]; then
    echo "image archive must contain exactly one manifest.json" >&2
    exit 1
  fi
  tar -xOf "$archive" manifest.json >"$destination"
}

manifest_config() {
  ruby -rjson -e '
    manifest = JSON.parse(File.read(ARGV.fetch(0)))
    abort "image archive must contain exactly one image manifest" unless manifest.length == 1
    config = manifest.fetch(0).fetch("Config")
    abort "unsafe image config path" unless /\A[0-9a-f]{64}\.json\z/.match?(config)
    puts config
  ' "$1"
}

create_artifact() {
  if (( $# != 5 )); then
    echo "usage: image-artifact.sh create IMAGE ARCHIVE METADATA SOURCE_SHA WORKFLOW_RUN_ID" >&2
    exit 2
  fi
  local image=$1 archive=$2 metadata=$3 source_sha=$4 workflow_run_id=$5
  [[ $source_sha =~ ^[0-9a-f]{40}$ ]] || {
    echo "source SHA must be 40 lowercase hexadecimal characters" >&2
    exit 1
  }
  [[ $workflow_run_id =~ ^([0-9]+|local)$ ]] || {
    echo "workflow run ID must be numeric or local" >&2
    exit 1
  }
  mkdir -p "$(dirname "$archive")" "$(dirname "$metadata")"
  local temporary
  temporary=$(mktemp -d "${TMPDIR:-/tmp}/pingora-image-artifact.XXXXXX")
  trap 'rm -rf "$temporary"' RETURN

  docker image save --output "$archive" "$image"
  extract_manifest "$archive" "$temporary/manifest.json" "$temporary/listing"
  local config_path config_sha image_id revision os architecture
  config_path=$(manifest_config "$temporary/manifest.json")
  [[ $(grep -Fxc "$config_path" "$temporary/listing") -eq 1 ]] || {
    echo "image archive config entry is missing or duplicated" >&2
    exit 1
  }
  tar -xOf "$archive" "$config_path" >"$temporary/config.json"
  config_sha=$(sha256_file "$temporary/config.json")
  image_id=$(docker image inspect --format '{{.Id}}' "$image")
  revision=$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image")
  os=$(docker image inspect --format '{{.Os}}' "$image")
  architecture=$(docker image inspect --format '{{.Architecture}}' "$image")
  [[ $image_id == "sha256:$config_sha" ]] || {
    echo "saved image config does not match the inspected image ID" >&2
    exit 1
  }
  [[ $revision == "$source_sha" ]] || {
    echo "OCI revision label does not match the verified source SHA" >&2
    exit 1
  }

  local metadata_tmp="${metadata}.tmp.$$"
  {
    echo "schema=1"
    echo "source_sha=$source_sha"
    echo "workflow_run_id=$workflow_run_id"
    echo "image_id=$image_id"
    echo "archive_sha256=$(sha256_file "$archive")"
    echo "manifest_sha256=$(sha256_file "$temporary/manifest.json")"
    echo "config_sha256=$config_sha"
    echo "os=$os"
    echo "architecture=$architecture"
    echo "revision_label=$revision"
  } >"$metadata_tmp"
  mv "$metadata_tmp" "$metadata"
}

verify_artifact() {
  if (( $# != 4 )); then
    echo "usage: image-artifact.sh verify ARCHIVE METADATA EXPECTED_SHA EXPECTED_WORKFLOW_RUN_ID" >&2
    exit 2
  fi
  local archive=$1 metadata=$2 expected_sha=$3 expected_run_id=$4
  [[ $expected_sha =~ ^[0-9a-f]{40}$ ]] || {
    echo "expected SHA must be 40 lowercase hexadecimal characters" >&2
    exit 1
  }
  [[ $expected_run_id =~ ^([0-9]+|local)$ ]] || {
    echo "expected workflow run ID must be numeric or local" >&2
    exit 1
  }
  local temporary
  temporary=$(mktemp -d "${TMPDIR:-/tmp}/pingora-image-artifact.XXXXXX")
  trap 'rm -rf "$temporary"' RETURN

  declare -A values=()
  while IFS='=' read -r key value; do
    case "$key" in
      schema|source_sha|workflow_run_id|image_id|archive_sha256|manifest_sha256|config_sha256|os|architecture|revision_label) ;;
      *) echo "unexpected image metadata key: $key" >&2; exit 1 ;;
    esac
    [[ -z ${values[$key]+set} ]] || {
      echo "duplicate image metadata key: $key" >&2
      exit 1
    }
    values[$key]=$value
  done <"$metadata"
  for key in schema source_sha workflow_run_id image_id archive_sha256 manifest_sha256 config_sha256 os architecture revision_label; do
    [[ -n ${values[$key]+set} ]] || {
      echo "missing image metadata key: $key" >&2
      exit 1
    }
  done
  [[ ${values[schema]} == 1 ]]
  [[ ${values[source_sha]} == "$expected_sha" ]]
  [[ ${values[workflow_run_id]} == "$expected_run_id" ]]
  [[ ${values[revision_label]} == "$expected_sha" ]]
  [[ ${values[image_id]} =~ ^sha256:[0-9a-f]{64}$ ]]
  [[ ${values[archive_sha256]} =~ ^[0-9a-f]{64}$ ]]
  [[ ${values[manifest_sha256]} =~ ^[0-9a-f]{64}$ ]]
  [[ ${values[config_sha256]} =~ ^[0-9a-f]{64}$ ]]
  [[ ${values[image_id]} == "sha256:${values[config_sha256]}" ]]
  [[ $(sha256_file "$archive") == "${values[archive_sha256]}" ]] || {
    echo "image archive checksum mismatch" >&2
    exit 1
  }

  extract_manifest "$archive" "$temporary/manifest.json" "$temporary/listing"
  [[ $(sha256_file "$temporary/manifest.json") == "${values[manifest_sha256]}" ]] || {
    echo "image manifest checksum mismatch" >&2
    exit 1
  }
  local config_path
  config_path=$(manifest_config "$temporary/manifest.json")
  [[ $(grep -Fxc "$config_path" "$temporary/listing") -eq 1 ]] || {
    echo "image archive config entry is missing or duplicated" >&2
    exit 1
  }
  tar -xOf "$archive" "$config_path" >"$temporary/config.json"
  [[ $(sha256_file "$temporary/config.json") == "${values[config_sha256]}" ]] || {
    echo "image config checksum mismatch" >&2
    exit 1
  }

  docker load --input "$archive" >/dev/null
  [[ $(docker image inspect --format '{{.Id}}' "${values[image_id]}") == "${values[image_id]}" ]]
  [[ $(docker image inspect --format '{{.Os}}' "${values[image_id]}") == "${values[os]}" ]]
  [[ $(docker image inspect --format '{{.Architecture}}' "${values[image_id]}") == "${values[architecture]}" ]]
  [[ $(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "${values[image_id]}") == "$expected_sha" ]]
  echo "verified_image_id=${values[image_id]}"
}

if (( $# < 1 )); then
  echo "usage: image-artifact.sh create|verify ..." >&2
  exit 2
fi
mode=$1
shift
case "$mode" in
  create) create_artifact "$@" ;;
  verify) verify_artifact "$@" ;;
  *) echo "unknown image artifact mode: $mode" >&2; exit 2 ;;
esac
