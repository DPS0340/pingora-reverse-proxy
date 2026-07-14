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
    allowed = /\A(?:[0-9a-f]{64}\.json|blobs\/sha256\/[0-9a-f]{64})\z/
    abort "unsafe image config path" unless allowed.match?(config)
    puts config
  ' "$1"
}

verify_saved_image_identity() {
  local archive=$1 listing=$2 temporary=$3 image_id=$4 config_sha=$5
  local image_hex=${image_id#sha256:}
  [[ $image_id =~ ^sha256:[0-9a-f]{64}$ ]] || {
    echo "saved image ID is not a sha256 digest" >&2
    exit 1
  }
  if [[ $image_hex == "$config_sha" ]]; then
    return
  fi

  local image_path="blobs/sha256/$image_hex"
  [[ $(grep -Fxc "$image_path" "$listing") -eq 1 ]] || {
    echo "saved OCI image identity blob is missing or duplicated" >&2
    exit 1
  }
  tar -xOf "$archive" "$image_path" >"$temporary/image-identity.json"
  [[ $(sha256_file "$temporary/image-identity.json") == "$image_hex" ]] || {
    echo "saved OCI image identity digest does not match its bytes" >&2
    exit 1
  }
  [[ $(grep -Fxc index.json "$listing") -eq 1 ]] || {
    echo "saved OCI image identity requires exactly one index.json" >&2
    exit 1
  }
  tar -xOf "$archive" index.json >"$temporary/index.json"
  ruby -rjson -e '
    index = JSON.parse(File.read(ARGV.fetch(0)))
    expected = ARGV.fetch(1)
    manifests = index.fetch("manifests")
    abort "saved OCI index does not reference the inspected image ID" unless
      manifests.count { |manifest| manifest["digest"] == expected } == 1
  ' "$temporary/index.json" "$image_id"
}

create_artifact() (
  if (( $# != 5 )); then
    echo "usage: image-artifact.sh create IMAGE ARCHIVE METADATA SOURCE_SHA WORKFLOW_RUN_ID" >&2
    exit 2
  fi
  local create_image=$1 create_archive=$2 create_metadata=$3 create_source=$4 create_run_id=$5
  [[ $create_source =~ ^[0-9a-f]{40}$ ]] || {
    echo "source SHA must be 40 lowercase hexadecimal characters" >&2
    exit 1
  }
  [[ $create_run_id =~ ^([0-9]+|local)$ ]] || {
    echo "workflow run ID must be numeric or local" >&2
    exit 1
  }
  mkdir -p "$(dirname "$create_archive")" "$(dirname "$create_metadata")"
  local temporary
  temporary=$(mktemp -d "${TMPDIR:-/tmp}/pingora-image-artifact.XXXXXX")
  trap 'rm -rf "$temporary"' EXIT INT TERM

  docker image save --output "$create_archive" "$create_image"
  extract_manifest "$create_archive" "$temporary/manifest.json" "$temporary/listing"
  local create_config_path create_config_sha create_image_id create_revision create_os create_architecture
  create_config_path=$(manifest_config "$temporary/manifest.json")
  [[ $(grep -Fxc "$create_config_path" "$temporary/listing") -eq 1 ]] || {
    echo "image archive config entry is missing or duplicated" >&2
    exit 1
  }
  tar -xOf "$create_archive" "$create_config_path" >"$temporary/config.json"
  create_config_sha=$(sha256_file "$temporary/config.json")
  create_image_id=$(docker image inspect --format '{{.Id}}' "$create_image")
  create_revision=$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$create_image")
  create_os=$(docker image inspect --format '{{.Os}}' "$create_image")
  create_architecture=$(docker image inspect --format '{{.Architecture}}' "$create_image")
  verify_saved_image_identity \
    "$create_archive" "$temporary/listing" "$temporary" "$create_image_id" "$create_config_sha"
  [[ $create_revision == "$create_source" ]] || {
    echo "OCI revision label does not match the verified source SHA" >&2
    exit 1
  }

  local metadata_tmp="${create_metadata}.tmp.$$"
  {
    echo "schema=1"
    echo "source_sha=$create_source"
    echo "workflow_run_id=$create_run_id"
    echo "image_id=$create_image_id"
    echo "archive_sha256=$(sha256_file "$create_archive")"
    echo "manifest_sha256=$(sha256_file "$temporary/manifest.json")"
    echo "config_sha256=$create_config_sha"
    echo "os=$create_os"
    echo "architecture=$create_architecture"
    echo "revision_label=$create_revision"
  } >"$metadata_tmp"
  mv "$metadata_tmp" "$create_metadata"
)

verify_artifact() (
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
  trap 'rm -rf "$temporary"' EXIT INT TERM

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
  verify_saved_image_identity \
    "$archive" "$temporary/listing" "$temporary" "${values[image_id]}" "${values[config_sha256]}"

  docker load --input "$archive" >/dev/null
  [[ $(docker image inspect --format '{{.Id}}' "${values[image_id]}") == "${values[image_id]}" ]]
  [[ $(docker image inspect --format '{{.Os}}' "${values[image_id]}") == "${values[os]}" ]]
  [[ $(docker image inspect --format '{{.Architecture}}' "${values[image_id]}") == "${values[architecture]}" ]]
  [[ $(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "${values[image_id]}") == "$expected_sha" ]]
  echo "verified_image_id=${values[image_id]}"
)

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
