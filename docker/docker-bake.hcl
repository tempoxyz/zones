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
  targets = ["tempo-zone", "tempo-zone-xtask"]
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
  inherits = ["docker-metadata"]
  dockerfile = "docker/Dockerfile.reproducible"
  context = "."
  target = "tempo-zone-reproducible"
  args = {
    SOURCE_DATE_EPOCH = "${SOURCE_DATE_EPOCH}"
    GIT_SHA = "${GIT_SHA}"
    VERSION = "${VERSION}"
  }
  labels = {
    "org.opencontainers.image.description" = "Production tempo-zone image; verification covers the tempo-zone binary, runtime execution config, and CA bundle."
    "org.tempoxyz.reproducible.verification-scope" = "tempo-zone-binary-runtime-config-ca-bundle"
  }
  platforms = ["linux/amd64"]
}

# Non-production candidate image for the manual reproducible-image verification
# workflow. The production target above uses the same binary build contract.
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

target "tempo-zone-prover-eif-builder" {
  dockerfile = "docker/Dockerfile.prover-eif-builder"
  context = "."
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
