#!/usr/bin/env ruby
# frozen_string_literal: true

SEMVER = /\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-((?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\z/

tag = ARGV.fetch(0) { abort "usage: release-policy.rb vSEMVER" }
abort "usage: release-policy.rb vSEMVER" unless ARGV.length == 1
abort "release tag must be v followed by strict SemVer 2.0" unless tag.start_with?("v")

version = tag.delete_prefix("v")
abort "release tag must be v followed by strict SemVer 2.0" unless SEMVER.match?(version)

# SemVer forbids underscores, so replacing the single build separator with an
# underscore is reversible and cannot collide with another valid SemVer.
docker_tag = version.tr("+", "_")
abort "mapped Docker tag is invalid" unless /\A[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}\z/.match?(docker_tag)

puts "release_tag=#{tag}"
puts "version=#{version}"
puts "docker_tag=#{docker_tag}"
