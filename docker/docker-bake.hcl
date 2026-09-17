variable "VERGEN_GIT_SHA" {
  default = ""
}

variable "VERGEN_GIT_SHA_SHORT" {
  default = ""
}

variable "SOURCE_DATE_EPOCH" {
  default = ""
}

variable "GIT_SHA" {
  default = ""
}

variable "VERSION" {
  default = "dev"
}

variable "PROVER_EIF_CONTEXT" {
  default = "./target/tempo-zone-prover-eif"
}

group "default" {
  targets = ["tempo-zone", "tempo-zone-xtask", "tempo-zone-prover-utils"]
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
  }
}

target "prover-chef" {
  dockerfile = "docker/Dockerfile.chef"
  context = "."
  platforms = ["linux/amd64"]
  args = {
    RUST_PROFILE = "release"
    RUST_FEATURES = ""
  }
}

target "_common" {
  dockerfile = "docker/Dockerfile"
  context = "."
  contexts = {
    chef = "target:chef"
  }
  args = {
    CHEF_IMAGE = "chef"
    RUST_PROFILE = "profiling"
    VERGEN_GIT_SHA = "${VERGEN_GIT_SHA}"
    VERGEN_GIT_SHA_SHORT = "${VERGEN_GIT_SHA_SHORT}"
  }
  platforms = ["linux/amd64"]
}

target "tempo-zone" {
  inherits = ["_common", "docker-metadata"]
  target = "tempo-zone"
}

# Non-production candidate image for the manual reproducible-image
# verification workflow. This uses Dockerfile.reproducible's dedicated build
# profile and flags, rather than the normal Dockerfile with a profile override.
target "tempo-zone-reproducible" {
  dockerfile = "docker/Dockerfile.reproducible"
  context = "."
  target = "tempo-zone-reproducible"
  args = {
    SOURCE_DATE_EPOCH = "${SOURCE_DATE_EPOCH}"
    GIT_SHA = "${GIT_SHA}"
    VERSION = "${VERSION}"
  }
  labels = {
    "org.opencontainers.image.description" = "Non-production reproducible candidate image; verification covers /usr/local/bin/tempo-zone only."
    "org.tempoxyz.reproducible.verification-scope" = "tempo-zone-binary"
  }
  platforms = ["linux/amd64"]
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
  }
  platforms = ["linux/amd64"]
}

# Build a matched Nitro guest kernel and NSM module from AWS's bootstrap sources. Keep this
# source pinned: changing it changes the EIF kernel and its PCR measurements.
target "nitro-enclaves-kernel" {
  context = "https://github.com/aws/aws-nitro-enclaves-sdk-bootstrap.git#f718dea60a9d9bb8b8682fd852ad793912f3c5db"
  target = "artifacts"
  args = {
    TARGET = "kernel"
  }
  platforms = ["linux/amd64"]
}

target "tempo-zone-prover-eif-builder" {
  dockerfile = "docker/Dockerfile.prover-eif-builder"
  context = "."
  contexts = {
    nitro-kernel = "target:nitro-enclaves-kernel"
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
