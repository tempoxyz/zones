variable "VERGEN_GIT_SHA" {
  default = ""
}

variable "VERGEN_GIT_SHA_SHORT" {
  default = ""
}

variable "PROVER_EIF_CONTEXT" {
  default = "./target/tempo-zone-prover-eif"
}

group "default" {
  targets = ["tempo-zone", "tempo-zone-xtask", "tempo-zone-prover-utils"]
}

group "prover-eif-inputs" {
  targets = ["tempo-zone-prover-enclave", "tempo-zone-prover-eif-builder"]
}

target "docker-metadata" {}

# Base image with all dependencies pre-compiled
target "chef" {
  dockerfile = "docker/Dockerfile.chef"
  context = "."
  platforms = ["linux/amd64"]
  args = {
    RUST_PROFILE = "profiling"
    RUST_FEATURES = "jemalloc"
    CACHE_FAMILY = "node"
    RUST_BINARIES = "tempo-zone tempo-xtask"
  }
}

target "prover-chef" {
  dockerfile = "docker/Dockerfile.chef"
  context = "."
  platforms = ["linux/amd64"]
  args = {
    RUST_PROFILE = "release"
    RUST_FEATURES = ""
    CACHE_FAMILY = "prover"
    RUST_BINARIES = "tempo-zone-prover-utils tempo-zone-prover-enclave"
  }
}

# Utilities and enclave share the same release dependency graph.
# Keep its layer and cache mounts reusable across both consumers.
target "_common" {
  dockerfile = "docker/Dockerfile"
  context = "."
  contexts = {
    chef = "target:chef"
  }
  args = {
    CHEF_IMAGE = "chef"
    RUST_PROFILE = "profiling"
    CACHE_FAMILY = "node"
    VERGEN_GIT_SHA = "${VERGEN_GIT_SHA}"
    VERGEN_GIT_SHA_SHORT = "${VERGEN_GIT_SHA_SHORT}"
  }
  platforms = ["linux/amd64"]
}

target "tempo-zone" {
  inherits = ["_common", "docker-metadata"]
  target = "tempo-zone"
}

target "tempo-zone-prover-enclave" {
  dockerfile = "docker/Dockerfile.prover-enclave"
  context = "."
  contexts = {
    chef = "target:prover-chef"
  }
  args = {
    CHEF_IMAGE = "chef"
    RUST_PROFILE = "release"
    CACHE_FAMILY = "prover"
  }
  platforms = ["linux/amd64"]
}

target "tempo-zone-prover-utils" {
  inherits = ["docker-metadata"]
  dockerfile = "docker/Dockerfile.prover-utils"
  context = "."
  contexts = {
    chef = "target:prover-chef"
  }
  args = {
    CHEF_IMAGE = "chef"
    RUST_PROFILE = "release"
    CACHE_FAMILY = "prover"
  }
  platforms = ["linux/amd64"]
}

# Build a hardened, matched set of Nitro bootstrap artifacts from pinned AWS sources. The local
# wrapper pins the Nix builder and applies the reviewed Zones kernel-config delta.
target "nitro-enclaves-bootstrap" {
  dockerfile = "docker/Dockerfile.nitro-enclaves-bootstrap"
  context = "."
  contexts = {
    nitro-bootstrap-source = "https://github.com/aws/aws-nitro-enclaves-sdk-bootstrap.git#f718dea60a9d9bb8b8682fd852ad793912f3c5db"
  }
  target = "artifacts"
  args = {
    TARGET = "all"
  }
  platforms = ["linux/amd64"]
}

target "tempo-zone-prover-eif-builder" {
  dockerfile = "docker/Dockerfile.prover-eif-builder"
  context = "."
  contexts = {
    nitro-bootstrap = "target:nitro-enclaves-bootstrap"
  }
  platforms = ["linux/amd64"]
}

# The EIF is generated from tempo-zone-prover-enclave before this target is
# built because Nitro CLI requires access to a local Docker image store.
target "tempo-zone-prover" {
  inherits = ["docker-metadata"]
  dockerfile = "docker/Dockerfile.prover-host"
  context = "."
  contexts = {
    prover-eif = "${PROVER_EIF_CONTEXT}"
  }
  platforms = ["linux/amd64"]
}

target "tempo-zone-xtask" {
  inherits = ["_common", "docker-metadata"]
  target = "tempo-zone-xtask"
}
