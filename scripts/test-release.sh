#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pingora-release-test.XXXXXX")
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

valid_tags=(
  v0.0.0
  v1.2.3
  v1.2.3-alpha
  v1.2.3-alpha.1+build.5
  v1.2.3+001
  v1.2.3-x-y-z.--
)
invalid_tags=(
  1.2.3
  v01.2.3
  v1.02.3
  v1.2.03
  v1.2
  v1.2.3-
  v1.2.3-alpha..1
  v1.2.3-01
  v1.2.3+
  v1.2.3+build..1
  v1.2.3+build_meta
  v1.2.3+build+again
)

declare -A docker_tags=()
for tag in "${valid_tags[@]}"; do
  output=$(ruby "$ROOT_DIR/scripts/release-policy.rb" "$tag")
  grep -Fqx "release_tag=$tag" <<<"$output"
  docker_tag=$(sed -n 's/^docker_tag=//p' <<<"$output")
  test -n "$docker_tag"
  [[ "$docker_tag" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]]
  if [[ -n ${docker_tags[$docker_tag]+set} ]]; then
    printf 'Docker tag collision: %s and %s -> %s\n' "${docker_tags[$docker_tag]}" "$tag" "$docker_tag" >&2
    exit 1
  fi
  docker_tags[$docker_tag]=$tag
done
test "${docker_tags[1.2.3-alpha.1_build.5]}" = "v1.2.3-alpha.1+build.5"
for tag in "${invalid_tags[@]}"; do
  if ruby "$ROOT_DIR/scripts/release-policy.rb" "$tag" >/dev/null 2>&1; then
    echo "invalid SemVer was accepted: $tag" >&2
    exit 1
  fi
done

mkdir -p "$TMP_DIR/archive/layer"
printf '{"architecture":"amd64","os":"linux"}\n' >"$TMP_DIR/archive/config.json"
config_sha=$(sha256sum "$TMP_DIR/archive/config.json" | cut -d' ' -f1)
mv "$TMP_DIR/archive/config.json" "$TMP_DIR/archive/${config_sha}.json"
printf 'layer-bytes\n' >"$TMP_DIR/archive/layer/layer.tar"
printf '[{"Config":"%s.json","RepoTags":["tested:image"],"Layers":["layer/layer.tar"]}]\n' \
  "$config_sha" >"$TMP_DIR/archive/manifest.json"
tar -cf "$TMP_DIR/image.tar" -C "$TMP_DIR/archive" manifest.json "${config_sha}.json" layer/layer.tar
archive_sha=$(sha256sum "$TMP_DIR/image.tar" | cut -d' ' -f1)
manifest_sha=$(sha256sum "$TMP_DIR/archive/manifest.json" | cut -d' ' -f1)
source_sha=$(printf '1%.0s' {1..40})
cat >"$TMP_DIR/metadata.env" <<EOF
schema=1
source_sha=$source_sha
workflow_run_id=local
image_id=sha256:$config_sha
archive_sha256=$archive_sha
manifest_sha256=$manifest_sha
config_sha256=$config_sha
os=linux
architecture=amd64
revision_label=$source_sha
EOF

mkdir "$TMP_DIR/bin"
cat >"$TMP_DIR/bin/docker" <<'DOCKER'
#!/usr/bin/env bash
set -euo pipefail
echo "$*" >>"$FAKE_DOCKER_LOG"
if [[ ${FAKE_DOCKER_INSPECT_FAILURE:-0} == 1 && "$1 $2" == "image inspect" ]]; then
  exit 51
fi
if [[ $1 == load ]]; then
  exit 0
fi
if [[ "$1 $2" != "image inspect" ]]; then
  exit 52
fi
format=$4
case "$format" in
  '{{.Id}}') echo "$FAKE_IMAGE_ID" ;;
  '{{.Os}}') echo linux ;;
  '{{.Architecture}}') echo amd64 ;;
  '{{index .Config.Labels "org.opencontainers.image.revision"}}') echo "$FAKE_SOURCE_SHA" ;;
  *) exit 53 ;;
esac
DOCKER
chmod 0755 "$TMP_DIR/bin/docker"

PATH="$TMP_DIR/bin:$PATH" \
  FAKE_DOCKER_LOG="$TMP_DIR/docker.log" \
  FAKE_IMAGE_ID="sha256:$config_sha" \
  FAKE_SOURCE_SHA="$source_sha" \
  "$ROOT_DIR/scripts/image-artifact.sh" verify \
    "$TMP_DIR/image.tar" "$TMP_DIR/metadata.env" "$source_sha" local
grep -Fqx 'load --input '"$TMP_DIR/image.tar" "$TMP_DIR/docker.log"
grep -Fq 'image inspect --format {{.Id}} sha256:' "$TMP_DIR/docker.log"

cp "$TMP_DIR/image.tar" "$TMP_DIR/corrupt.tar"
printf 'corruption' >>"$TMP_DIR/corrupt.tar"
if PATH="$TMP_DIR/bin:$PATH" \
  FAKE_DOCKER_LOG="$TMP_DIR/corrupt-docker.log" \
  FAKE_IMAGE_ID="sha256:$config_sha" \
  FAKE_SOURCE_SHA="$source_sha" \
  "$ROOT_DIR/scripts/image-artifact.sh" verify \
    "$TMP_DIR/corrupt.tar" "$TMP_DIR/metadata.env" "$source_sha" local >/dev/null 2>&1; then
  echo "corrupt image archive was accepted" >&2
  exit 1
fi

if PATH="$TMP_DIR/bin:$PATH" \
  FAKE_DOCKER_LOG="$TMP_DIR/inspect-docker.log" \
  FAKE_DOCKER_INSPECT_FAILURE=1 \
  FAKE_IMAGE_ID="sha256:$config_sha" \
  FAKE_SOURCE_SHA="$source_sha" \
  "$ROOT_DIR/scripts/image-artifact.sh" verify \
    "$TMP_DIR/image.tar" "$TMP_DIR/metadata.env" "$source_sha" local >/dev/null 2>&1; then
  echo "Docker inspection failure was accepted" >&2
  exit 1
fi

grep -Fq "name: verified-image-\${{ github.sha }}" "$ROOT_DIR/.github/workflows/ci.yml"
grep -Fq 'CONTAINER_GATE_IMAGE_ARCHIVE:' "$ROOT_DIR/.github/workflows/ci.yml"
grep -Fq "run-id: \${{ github.event.workflow_run.id }}" "$ROOT_DIR/.github/workflows/cd.yml"
grep -Fq "name: verified-image-\${{ github.event.workflow_run.head_sha }}" "$ROOT_DIR/.github/workflows/cd.yml"
grep -Fq 'scripts/image-artifact.sh verify' "$ROOT_DIR/.github/workflows/cd.yml"
if grep -Eq 'docker (build([[:space:]]|$)|buildx build([[:space:]]|$))' "$ROOT_DIR/.github/workflows/cd.yml"; then
  echo "CD rebuilds instead of promoting the tested image" >&2
  exit 1
fi
if grep -Eq '(^|[^[:alnum:]_-])latest([^[:alnum:]_-]|$)' "$ROOT_DIR/.github/workflows/cd.yml"; then
  echo "CD publishes latest" >&2
  exit 1
fi
grep -Fq 'group: pingora-container-publication' "$ROOT_DIR/.github/workflows/cd.yml"
grep -Fq 'git fetch --force --no-tags origin' "$ROOT_DIR/.github/workflows/cd.yml"
grep -Fq 'docker buildx imagetools inspect' "$ROOT_DIR/.github/workflows/cd.yml"
grep -Fq 'actions: read' "$ROOT_DIR/.github/workflows/cd.yml"
if grep -Eh '^[[:space:]]*uses:' "$ROOT_DIR/.github/workflows/ci.yml" "$ROOT_DIR/.github/workflows/cd.yml" | \
  grep -Ev '@[0-9a-f]{40}([[:space:]]|$)'; then
  echo "workflow action is not pinned to a full SHA" >&2
  exit 1
fi

printf 'release gate passed: strict SemVer, collision-free tags, exact archive identity, and no-rebuild promotion\n'
