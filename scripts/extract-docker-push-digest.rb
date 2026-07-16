#!/usr/bin/env ruby
# frozen_string_literal: true

abort "usage: #{File.basename($PROGRAM_NAME)} DOCKER_PUSH_LOG" unless ARGV.length == 1

digests = File.foreach(ARGV.fetch(0)).each_with_object([]) do |line, matches|
  match = /\A\S+: digest: (sha256:[0-9a-f]{64}) size: [0-9]+\s*\z/.match(line)
  matches << match[1] if match
end

abort "expected exactly one Docker push digest line, found #{digests.length}" unless digests.length == 1

puts digests.fetch(0)
