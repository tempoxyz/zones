# Source images for prover E2E

Docker CI publishes `tempo-zone`, `tempo-zone-xtask`, and `tempo-zone-prover-utils`
under `source-<full candidate SHA>` tags. Same-repository PRs targeting `main`
publish only those candidate tags; ordinary release/main tags retain their existing
behavior. Fork PRs do not publish images. The candidate is the commit actually
checked out by CI, including GitHub's PR merge or merge-group commit.

For a per-genesis devnet, dispatch `docker.yml` on an empty child commit of the
candidate with `source_sha=<full candidate SHA>` and the test's `tempo_genesis_url`.
The workflow checks the parent and source tree before reading source images. It
pins available source digests, compiles only missing images with the candidate's
version SHA, and aliases the results under the child's ordinary `sha-` tags. Each
alias must resolve to the same digest as its source. Provisioning can therefore
keep consuming the per-run tags and digests without recompiling node/xtask.

The enclave payload, EIF, and prover host package still use the test's genesis;
their final image and PCR artifact remain keyed by the child commit. L1 remains
the separately built `tempo-devnet` image with `custom-pcrs` enabled. No production
verifier policy changes are needed.

Reuse is optional: ordinary CI keeps compiling each candidate. A new merge-group
SHA needs its first source build, and simultaneous misses can build concurrently.
Registry authentication/network errors fail explicitly; only absent manifests
trigger a build fallback. All consumers pin digests once resolved.

Run `node --test .github/scripts/source-images.test.cjs` for the source-tree,
cache-miss, provenance, PR-tag, and digest-preservation checks.
