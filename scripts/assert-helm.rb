#!/usr/bin/env ruby
# frozen_string_literal: true

require "yaml"

case_name = ARGV.fetch(0)
documents = YAML.load_stream($stdin.read).compact

def assert(condition, message)
  raise message unless condition
end

def resource(documents, kind, suffix = nil)
  matches = documents.select { |document| document["kind"] == kind }
  matches = matches.select { |document| document.dig("metadata", "name").end_with?(suffix) } if suffix
  raise "missing or ambiguous #{kind} #{suffix}" unless matches.length == 1

  matches.fetch(0)
end

deployment = resource(documents, "Deployment")
pod_spec = deployment.dig("spec", "template", "spec")
container = pod_spec.fetch("containers").fetch(0)
args = container.fetch("args")
env = container.fetch("env").to_h { |entry| [entry.fetch("name"), entry] }

assert(pod_spec["automountServiceAccountToken"] == false, "pod service-account token automount must be disabled")
assert(pod_spec.dig("securityContext", "runAsNonRoot") == true, "pod must run as non-root")
assert(pod_spec.dig("securityContext", "runAsUser") == 65_532, "pod UID must be 65532")
assert(pod_spec.dig("securityContext", "runAsGroup") == 65_532, "pod GID must be 65532")
assert(pod_spec.dig("securityContext", "seccompProfile", "type") == "RuntimeDefault", "pod seccomp must be RuntimeDefault")
assert(container.dig("securityContext", "allowPrivilegeEscalation") == false, "privilege escalation must be disabled")
assert(container.dig("securityContext", "readOnlyRootFilesystem") == true, "root filesystem must be read-only")
assert(container.dig("securityContext", "capabilities", "drop") == ["ALL"], "all capabilities must be dropped")
assert(container.dig("securityContext", "runAsNonRoot") == true, "container must run as non-root")
assert(container.dig("securityContext", "runAsUser") == 65_532, "container UID must be 65532")
assert(container.dig("securityContext", "runAsGroup") == 65_532, "container GID must be 65532")
assert(container.fetch("image") !~ /:latest(?:@|$)/, "image must not use latest")
assert(deployment.dig("spec", "replicas") == 1, "chart must deploy exactly one replica")
assert(deployment.dig("spec", "strategy", "type") == "Recreate", "deployment strategy must be Recreate")

auth = env.fetch("CONFIGPROXY_AUTH_TOKEN")
assert(auth.key?("valueFrom") && !auth.key?("value"), "API token must come from a Secret ref")
assert(auth.dig("valueFrom", "secretKeyRef", "name"), "API Secret name is missing")
assert(auth.dig("valueFrom", "secretKeyRef", "key"), "API Secret key is missing")
assert(env.dig("PINGORA_REQUIRE_AUTH_TOKEN", "value") == "true", "chart must require management authentication")

expected_listener_args = %w[
  --ip 0.0.0.0 --port 8000
  --api-ip 0.0.0.0 --api-port 8001
  --metrics-ip 0.0.0.0 --metrics-port 8002
]
expected_listener_args.each { |argument| assert(args.include?(argument), "listener arg missing: #{argument}") }

if case_name == "tls"
  %w[startupProbe readinessProbe livenessProbe].each do |probe|
    assert(!container.key?(probe), "#{probe} must be disabled for mandatory public client certificates")
  end
else
  assert(container.dig("startupProbe", "httpGet", "path") == "/_chp_healthz", "startup probe path is wrong")
  assert(container.dig("readinessProbe", "httpGet", "path") == "/_chp_healthz", "readiness probe path is wrong")
  assert(container.dig("livenessProbe", "httpGet", "path") == "/_chp_healthz", "liveness probe path is wrong")
  %w[startupProbe readinessProbe livenessProbe].each do |probe|
    assert(container.dig(probe, "httpGet", "port") == "public", "#{probe} must use the public listener")
    assert(container.dig(probe, "timeoutSeconds").to_i.positive?, "#{probe} timeout must be bounded")
  end
end

tmp_volume = pod_spec.fetch("volumes").find { |volume| volume["name"] == "tmp" }
assert(tmp_volume&.key?("emptyDir"), "tmp emptyDir is missing")
tmp_mount = container.fetch("volumeMounts").find { |mount| mount["name"] == "tmp" }
assert(tmp_mount&.fetch("mountPath") == "/tmp", "tmp mount is missing")
assert(tmp_mount["readOnly"] != true, "tmp must be writable")
container.fetch("volumeMounts").reject { |mount| mount["name"] == "tmp" }.each do |mount|
  assert(mount["readOnly"] == true, "only /tmp may be writable")
end

resources = container.fetch("resources")
assert(resources.dig("requests", "cpu"), "CPU request is missing")
assert(resources.dig("requests", "memory"), "memory request is missing")
assert(resources.dig("limits", "cpu"), "CPU limit is missing")
assert(resources.dig("limits", "memory"), "memory limit is missing")

resource(documents, "Service", "-public")
api_service = resource(documents, "Service", "-api")
resource(documents, "Service", "-metrics")
assert(api_service.dig("spec", "type") == "ClusterIP", "API service must default to ClusterIP")

case case_name
when "memory", "resources", "upgrade"
  index = args.index("--storage-backend")
  assert(index && args[index + 1] == "memory", "memory backend arg is missing")
when "redis"
  index = args.index("--storage-backend")
  assert(index && args[index + 1] == "redis", "Redis backend arg is missing")
  redis = env.fetch("PINGORA_REDIS_URL")
  assert(redis.key?("valueFrom") && !redis.key?("value"), "Redis URL must come from a Secret ref")
  assert(redis.dig("valueFrom", "secretKeyRef", "name") == "task13-redis", "Redis Secret name is wrong")
when "sidecar"
  index = args.index("--storage-backend")
  assert(index && args[index + 1] == "sidecar", "sidecar backend arg is missing")
  assert(env.dig("PINGORA_SIDECAR_URL", "value") == "http://route-store.default.svc:8080/", "sidecar endpoint is wrong")
  token = env.fetch("PINGORA_SIDECAR_BEARER_TOKEN")
  assert(token.key?("valueFrom") && !token.key?("value"), "sidecar token must come from a Secret ref")
when "tls"
  %w[--ssl-key --ssl-cert --ssl-ca --ssl-request-cert --ssl-reject-unauthorized --api-ssl-key --api-ssl-cert --api-ssl-ca --api-ssl-request-cert --api-ssl-reject-unauthorized --client-ssl-key --client-ssl-cert --client-ssl-ca].each do |argument|
    assert(args.include?(argument), "TLS arg missing: #{argument}")
  end
  secret_volumes = pod_spec.fetch("volumes").select { |volume| volume.key?("secret") }
  assert(secret_volumes.length == 4, "expected public, API, client identity, and upstream CA Secret volumes")
when "upstream-ca"
  assert(args.include?("--client-ssl-ca"), "upstream CA arg is missing")
  assert(!args.include?("--client-ssl-key"), "CA-only trust must not require a client key")
  assert(!args.include?("--client-ssl-cert"), "CA-only trust must not require a client certificate")
when "digest"
  assert(
    container.fetch("image") == "registry.example/pingora-reverse-proxy@sha256:#{'a' * 64}",
    "digest image reference is not exact"
  )
end

assert(documents.none? { |document| document["kind"] == "Secret" }, "chart must not render secret values")
