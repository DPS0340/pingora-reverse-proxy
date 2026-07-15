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

push_digest=$(printf 'a%.0s' {1..64})
printf 'The push refers to repository [example.invalid/image]\ncandidate-staging: digest: sha256:%s size: 1234\n' \
  "$push_digest" >"$TMP_DIR/push-valid.log"
test "$(ruby "$ROOT_DIR/scripts/extract-docker-push-digest.rb" "$TMP_DIR/push-valid.log")" = \
  "sha256:$push_digest"
printf 'push completed without a digest line\n' >"$TMP_DIR/push-missing.log"
if ruby "$ROOT_DIR/scripts/extract-docker-push-digest.rb" "$TMP_DIR/push-missing.log" >/dev/null 2>&1; then
  echo "missing Docker push digest was accepted" >&2
  exit 1
fi
cat "$TMP_DIR/push-valid.log" "$TMP_DIR/push-valid.log" >"$TMP_DIR/push-ambiguous.log"
if ruby "$ROOT_DIR/scripts/extract-docker-push-digest.rb" "$TMP_DIR/push-ambiguous.log" >/dev/null 2>&1; then
  echo "ambiguous Docker push digests were accepted" >&2
  exit 1
fi

mkdir -p "$TMP_DIR/archive/layer" "$TMP_DIR/archive/blobs/sha256"
printf '{"architecture":"amd64","os":"linux"}\n' >"$TMP_DIR/archive/config.json"
config_sha=$(sha256sum "$TMP_DIR/archive/config.json" | cut -d' ' -f1)
mv "$TMP_DIR/archive/config.json" "$TMP_DIR/archive/blobs/sha256/${config_sha}"
printf 'layer-bytes\n' >"$TMP_DIR/archive/layer/layer.tar"
printf '[{"Config":"blobs/sha256/%s","RepoTags":["tested:image"],"Layers":["layer/layer.tar"]}]\n' \
  "$config_sha" >"$TMP_DIR/archive/manifest.json"
printf '{"schemaVersion":2,"manifests":[]}' >"$TMP_DIR/archive/image-index.json"
image_sha=$(sha256sum "$TMP_DIR/archive/image-index.json" | cut -d' ' -f1)
mv "$TMP_DIR/archive/image-index.json" "$TMP_DIR/archive/blobs/sha256/${image_sha}"
printf '{"schemaVersion":2,"manifests":[{"digest":"sha256:%s"}]}' \
  "$image_sha" >"$TMP_DIR/archive/index.json"
tar -cf "$TMP_DIR/image.tar" -C "$TMP_DIR/archive" \
  manifest.json index.json "blobs/sha256/${image_sha}" "blobs/sha256/${config_sha}" layer/layer.tar
archive_sha=$(sha256sum "$TMP_DIR/image.tar" | cut -d' ' -f1)
manifest_sha=$(sha256sum "$TMP_DIR/archive/manifest.json" | cut -d' ' -f1)
source_sha=$(printf '1%.0s' {1..40})
cat >"$TMP_DIR/metadata.env" <<EOF
schema=1
source_sha=$source_sha
workflow_run_id=local
image_id=sha256:$image_sha
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
  FAKE_IMAGE_ID="sha256:$image_sha" \
  FAKE_SOURCE_SHA="$source_sha" \
  "$ROOT_DIR/scripts/image-artifact.sh" verify \
    "$TMP_DIR/image.tar" "$TMP_DIR/metadata.env" "$source_sha" local
grep -Fqx 'load --input '"$TMP_DIR/image.tar" "$TMP_DIR/docker.log"
grep -Fq 'image inspect --format {{.Id}} sha256:' "$TMP_DIR/docker.log"

cp "$TMP_DIR/image.tar" "$TMP_DIR/corrupt.tar"
printf 'corruption' >>"$TMP_DIR/corrupt.tar"
if PATH="$TMP_DIR/bin:$PATH" \
  FAKE_DOCKER_LOG="$TMP_DIR/corrupt-docker.log" \
  FAKE_IMAGE_ID="sha256:$image_sha" \
  FAKE_SOURCE_SHA="$source_sha" \
  "$ROOT_DIR/scripts/image-artifact.sh" verify \
    "$TMP_DIR/corrupt.tar" "$TMP_DIR/metadata.env" "$source_sha" local >/dev/null 2>&1; then
  echo "corrupt image archive was accepted" >&2
  exit 1
fi

if PATH="$TMP_DIR/bin:$PATH" \
  FAKE_DOCKER_LOG="$TMP_DIR/inspect-docker.log" \
  FAKE_DOCKER_INSPECT_FAILURE=1 \
  FAKE_IMAGE_ID="sha256:$image_sha" \
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
grep -Fqx 'channel = "1.85.1"' "$ROOT_DIR/rust-toolchain.toml"
grep -Fq 'PROPTEST_CASES=4096' "$ROOT_DIR/scripts/verify.sh"
ruby -ryaml - "$ROOT_DIR/.github/workflows/ci.yml" "$ROOT_DIR/.github/workflows/cd.yml" <<'RUBY'
def assert(condition, message)
  raise message unless condition
end

ci = YAML.safe_load(File.read(ARGV.fetch(0)), aliases: true)
cd = YAML.safe_load(File.read(ARGV.fetch(1)), aliases: true)
linux = ci.fetch('jobs').fetch('verify-linux')
linux_env = linux.fetch('env')
archive = '/tmp/pingora-verified-image/image.tar'
metadata = '/tmp/pingora-verified-image/metadata.env'
assert(linux_env.fetch('CONTAINER_GATE_IMAGE_ARCHIVE') == archive,
       'authoritative artifact archive path must be a static Linux-safe absolute path')
assert(linux_env.fetch('CONTAINER_GATE_IMAGE_METADATA') == metadata,
       'authoritative artifact metadata path must be a static Linux-safe absolute path')

linux_steps = linux.fetch('steps')
install_index = linux_steps.index { |step| step['name'] == 'Install actionlint' }
lint_index = linux_steps.index { |step| step['name'] == 'Check workflow schemas' }
tools_index = linux_steps.index { |step| step['name'] == 'Install native and release-gate tools' }
verify_index = linux_steps.index { |step| step['name'] == 'Run authoritative release gate' }
assert(install_index && lint_index && tools_index && verify_index &&
       install_index < lint_index && lint_index < tools_index && tools_index < verify_index,
       'schema checking and exact release-tool installation must precede the authoritative gate')
install_script = linux_steps.fetch(install_index).fetch('run')
assert(install_script.include?('actionlint_1.7.12_linux_amd64.tar.gz'),
       'actionlint install must pin the Linux amd64 v1.7.12 archive')
assert(install_script.include?('8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8'),
       'actionlint archive checksum must match the official v1.7.12 release')
tools_script = linux_steps.fetch(tools_index).fetch('run')
assert(tools_script.include?('rustup toolchain install 1.85.1 --profile minimal --component clippy,rustfmt') &&
       tools_script.include?('rustup toolchain install 1.89.0 --profile minimal') &&
       tools_script.include?('cargo +1.89.0 install --locked --version 1.56.0 just') &&
       tools_script.include?('cargo +1.89.0 install --locked --version 0.22.2 cargo-audit') &&
       tools_script.include?('cargo +1.89.0 install --locked --version 0.20.2 cargo-deny'),
       'release tools with newer MSRVs must build under an exact isolated toolchain')
linux_toolchain = linux_steps.find { |step| step['name'] == 'Record resolved Rust toolchain' }.fetch('run')
assert(linux_toolchain.include?('rustc 1.85.1 (4eb161250 2025-03-15)'),
       'authoritative Linux source gate must reject any compiler other than Rust 1.85.1')
macos_steps = ci.fetch('jobs').fetch('rust-macos').fetch('steps')
macos_install = macos_steps.find { |step| step['name'] == 'Install required native tools' }.fetch('run')
assert(macos_install.include?('rustup toolchain install 1.85.1 --profile minimal --component clippy,rustfmt'),
       'fresh macOS runners must install the exact project toolchain before source checks')
macos_toolchain = macos_steps.find { |step| step['name'] == 'Record resolved Rust toolchain' }.fetch('run')
assert(macos_toolchain.include?('rustc 1.85.1 (4eb161250 2025-03-15)'),
       'macOS source checks must reject any compiler other than Rust 1.85.1')
assert(linux_steps.fetch(lint_index).fetch('run').include?('actionlint .github/workflows/ci.yml .github/workflows/cd.yml'),
       'authoritative workflow schema check must cover CI and CD')

upload = linux_steps.find { |step| step['name'] == 'Upload exact verified production image' }
upload_paths = upload.fetch('with').fetch('path').lines.map(&:strip).reject(&:empty?)
assert(upload_paths == ['${{ env.CONTAINER_GATE_IMAGE_ARCHIVE }}',
                        '${{ env.CONTAINER_GATE_IMAGE_METADATA }}'],
       'artifact upload must consume the same archive and metadata paths as verification')

concurrency = cd.fetch('concurrency')
assert(concurrency.keys.sort == ['cancel-in-progress', 'group'],
       'publication concurrency must use only supported group and cancel-in-progress keys')
assert(concurrency.fetch('group') == 'pingora-container-publication' &&
       concurrency.fetch('cancel-in-progress') == false,
       'publication must remain serialized without canceling an active publication')

publish_steps = cd.fetch('jobs').fetch('publish').fetch('steps')
download = publish_steps.find { |step| step['name'] == 'Download exact image from the verified workflow run' }
assert(download.fetch('with').fetch('path') == '${{ runner.temp }}/pingora-verified-image',
       'download path must preserve the verified artifact directory contract')
candidate = publish_steps.find { |step| step['name'] == 'Verify and load the tested image bytes' }
candidate_script = candidate.fetch('run')
assert(candidate_script.include?('${RUNNER_TEMP}/pingora-verified-image/image.tar') &&
       candidate_script.include?('${RUNNER_TEMP}/pingora-verified-image/metadata.env'),
       'CD verification must consume the downloaded archive and metadata paths')

candidate_publish_index = publish_steps.index { |step| step['name'] == 'Publish bounded staging manifest' }
attest_index = publish_steps.index { |step| step['name'] == 'Attest candidate image provenance' }
promotion_index = publish_steps.index { |step| step['name'] == 'Publish attested digest to release discovery tags' }
cleanup_index = publish_steps.index { |step| step['name'] == 'Clean up registry credentials' }
assert(candidate_publish_index && attest_index && promotion_index && cleanup_index &&
       candidate_publish_index < attest_index && attest_index < promotion_index &&
       promotion_index < cleanup_index,
       'candidate publication, attestation, promotion, and credential cleanup must remain ordered')
candidate_publish = publish_steps.fetch(candidate_publish_index)
candidate_publish_script = candidate_publish.fetch('run')
candidate_publish_env = candidate_publish.fetch('env')
attest = publish_steps.fetch(attest_index)
promotion_script = publish_steps.fetch(promotion_index).fetch('run')
cleanup = publish_steps.fetch(cleanup_index)
assert(!candidate_publish_env.key?('PUBLICATION_RUN_ID') &&
       !candidate_publish_env.key?('PUBLICATION_RUN_ATTEMPT'),
       'serialized publication must not create an unbounded candidate tag per workflow attempt')
assert(candidate_publish_script.include?('candidate_tag="candidate-staging"') &&
       candidate_publish_script.include?('docker push "${image}:${candidate_tag}" 2>&1 | tee "${push_output_file}"'),
       'candidate publication must reuse one bounded internal staging tag and retain push output')
assert(candidate_publish_script.include?('candidate_digest=$(ruby') &&
       candidate_publish_script.include?('scripts/extract-docker-push-digest.rb "${push_output_file}"') &&
       candidate_publish_script.include?('"${image}@${candidate_digest}"') &&
       !candidate_publish_script.include?('remote_digest "${image}:${candidate_tag}"'),
       'attested digest must come directly from the successful push result and be verified by digest')
assert(!candidate_publish_script.include?('require_absent_tag "${image}:${VERSION_TAG}"') &&
       !candidate_publish_script.include?('require_absent_tag "${image}:${sha_tag}"'),
       'a rerun must reach idempotent promotion after a partially completed prior promotion')
assert(!candidate_publish_script.include?('--tag "${image}:${VERSION_TAG}"') &&
       !candidate_publish_script.include?('--tag "${image}:${sha_tag}"'),
       'candidate publication must not expose public release tags before attestation')
assert(attest.fetch('env').fetch('DOCKER_CONFIG') == '${{ runner.temp }}/pingora-docker-config',
       'registry credentials must remain available while provenance is pushed')
assert(promotion_script.include?('publish_discovery_tag "${image}:${VERSION_TAG}"') &&
       promotion_script.include?('publish_discovery_tag "${image}:${sha_tag}"') &&
       promotion_script.include?('test "$(remote_digest "${reference}")" = "${ATTESTED_DIGEST}"') &&
       promotion_script.include?('"${image}@${ATTESTED_DIGEST}"'),
       'discovery tag publication must use and verify only the attested digest')
assert(!candidate_publish_script.match?(/manifest unknown|not found: manifest|no such manifest|set \+e/) &&
       !promotion_script.match?(/manifest unknown|not found: manifest|no such manifest|set \+e/),
       'registry failures must never be parsed as an absent-tag authorization path')
assert(cleanup.fetch('if') == '${{ always() }}' &&
       cleanup.fetch('env').fetch('DOCKER_CONFIG') == '${{ runner.temp }}/pingora-docker-config' &&
       cleanup.fetch('run').include?('rm -rf -- "${DOCKER_CONFIG}"'),
       'registry credentials must be removed on every success or failure path')
RUBY
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

printf 'release gate passed: strict SemVer, bounded staging publication, push-result digest capture, exact archive identity, provenance-before-discovery-tags, digest verification, and no rebuild\n'
