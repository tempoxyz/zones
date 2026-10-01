# Post-link node revision stamping

Container builds compile and link `tempo-zone` without a Git revision. The fixed
`ZONE_STAMP_REVISION=1` setting selects a 40-byte `.tempo_revision` ELF section;
ordinary Cargo builds retain their existing compile-time version metadata.
`.git` is excluded from the Docker context. Only the `stamped-node` packaging
stage accepts `VERGEN_GIT_SHA`, so identical build inputs with a different SHA can
reuse the complete compilation layer, not just cached Cargo dependencies.

`stamp-revision.sh` validates a full lowercase Git SHA and an unstamped section,
then uses GNU objcopy to replace its contents without invoking Cargo or a linker.
Version initialization reads the section with a volatile read to prevent constant
folding, including under LTO. The SHA is used consistently for CLI, RPC and peer
identity. The extracted executable is self-contained: no environment variable or
sidecar file is needed. Unstamped binaries fail version initialization.

Stamping is Linux/ELF-only and happens before artifact checksums or signing. The
linker's build ID still identifies the underlying linked code/debug artifact;
the final checksum identifies the stamped executable. Neither this stamp nor
the matching OCI revision label is cryptographic proof of source. Compilation
timestamps remain those of the cached compilation, not repackaging. Container
versions retain the `-dev` suffix (no Git tag metadata in their build context).

The xtask image receives the OCI label only; xtask does not initialize the node's
CLI version metadata. Prover/EIF/PCR generation and Tempo's custom-pcrs builds
are unchanged. This does not add a persistent Cargo target cache or accelerate
compilation after actual source changes.

Run `bash docker/tests/stamp-revision.sh` for standalone optimized/LTO/stripping
tests. The Docker Revision Cache workflow compares SHA-only warm rebuilds using
the old compile-time Dockerfile and the new stamping Dockerfile, checks BuildKit
cache hits, and runs the extracted real binaries on the runner. Logs, timings
and standalone version output are retained as workflow artifacts.
