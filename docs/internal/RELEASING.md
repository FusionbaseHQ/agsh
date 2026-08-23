# Releasing agsh

Releases are built by [`.github/workflows/release.yml`](../../.github/workflows/release.yml):
pushing a stable `vMAJOR.MINOR.PATCH` tag runs the full two-platform gate (fmt,
clippy, cargo tests, golden checks, bash/sh differential tests, and PTY
interactive tests), builds
four targets on native runners (macOS 15 arm64/x86_64, Ubuntu 22.04
x86_64/aarch64 with static-musl shell binaries), and builds the optional
deep-interception library (native macOS or glibc Linux). Separate protected jobs
then **sign and notarize all three shipped macOS Mach-O files** (the shell,
raw-exec launch helper, and interception library) and publish a GitHub
Release with binary tarballs, a complete corresponding-source tarball, the
installer, `checksums.txt`, and notes extracted from `CHANGELOG.md`.
`workflow_dispatch` always dry-runs the build and source-verification matrix
without signing, attesting, or publishing, even when the selected ref is a tag.

Each binary archive includes the AGPL license, project and rtk attribution,
the rtk Apache-2.0 text, and the lockfile-derived Rust dependency license
bundle. It also copies the pinned toolchain's generated Rust standard-library
copyright/license inventory, which is not represented in `Cargo.lock`. Linux
archives additionally copy `/usr/share/doc/musl/copyright` from the Ubuntu musl
linker package as `MUSL_COPYRIGHT.txt`. The
release workflow creates `agsh-vX.Y.Z-source.tar.gz`: the exact
tagged repository tree plus every locked, non-system Rust dependency under
`vendor/` and a Cargo source configuration for offline builds. This keeps the
AGPL Corresponding Source available beside the object-code assets rather than
depending on crates.io remaining available indefinitely.

The tag test gate also runs the installer fixture on both Ubuntu and macOS.
Those cases inject deterministic Linux and Darwin platform responses and serve
archives through a fake `curl`, so they validate the target-specific archive,
version-coupled launch helper, interception-library, and notice branches without
contacting a release service.

After the source archive is created, a separate job with read-only repository
permission and no signing or publication credentials downloads that exact
artifact, verifies its sidecar, extracts it into a clean directory, and installs
the pinned toolchain. It then drops back to the runner UID/GID, disallows new
privileges, and runs `cargo build --workspace --release --locked --offline`
inside a new Linux network namespace whose only interface is a down loopback
device. Cargo, build scripts, proc macros, and compiler subprocesses therefore
cannot fetch undeclared build inputs. The pinned Rust toolchain remains a
general-purpose build tool selected by `rust-toolchain.toml` and is installed
before network isolation begins. The verifier resolves the installed Cargo and
Rust compiler executables before entering the namespace, so rustup proxies cannot
attempt a component sync after networking has been removed.

## Cutting a release

Only stable `vMAJOR.MINOR.PATCH` releases are supported. Prerelease and build
metadata tags are rejected rather than being published as ordinary/latest
GitHub releases.

1. Update `CHANGELOG.md`: move `[Unreleased]` content under `## [X.Y.Z] - date`.
2. Bump `version` in the workspace `Cargo.toml` **and** the internal path-dep
   `version = "…"` requirements (grep for the old version across
   `crates/*/Cargo.toml`).
3. If `Cargo.lock` changed, install the pinned maintainer tool described by
   `scripts/generate-third-party-licenses.sh`, then run that script. It operates
   offline and records a digest of `Cargo.lock`, every package manifest, and the
   license configuration/template in `THIRD_PARTY_LICENSES.html`, plus a digest
   of the generated document in `THIRD_PARTY_LICENSES.html.sha256`.
4. Run `scripts/validate-release.sh`, commit, push, and wait for CI green.
5. Review repository Dependabot alerts and run `cargo audit` with a freshly
   updated RustSec advisory database. Record any accepted exception and expiry
   in the release PR; the repository does not currently have an offline
   vulnerability database to make this a deterministic build step.
6. Optional: `gh workflow run release.yml --ref main` to dry-run the matrix.
7. `git tag -a vX.Y.Z -m "agsh vX.Y.Z" && git push origin vX.Y.Z` — done.

Before the first release:

1. Configure repository rules so `main` requires the CI workflow, only release
   maintainers can create `v*` tags, and existing `v*` tags cannot be updated or
   deleted. The release validator refuses a tag whose commit is not already
   reachable from `origin/main`, and the publisher re-resolves the annotated tag
   immediately before attestation and publication.
2. Enable **release immutability** under repository Settings > General >
   Releases. With an authenticated maintainer token that has Administration-read
   permission, verify the required setting before pushing the first tag:

   ```sh
   test "$(gh api repos/FusionbaseHQ/agsh/immutable-releases \
     -H 'X-GitHub-Api-Version: 2026-03-10' \
     --jq .enabled)" = true
   ```

   The publication token intentionally has no repository-administration access,
   so the workflow cannot perform this preflight itself. It does check the
   published release's `isImmutable` result and raises a configuration alarm if
   the setting was removed, but that post-publication alarm is not a substitute
   for this maintainer preflight.
3. Create `release-signing` and `release-publication` GitHub Environments. GitHub
   otherwise creates a referenced environment without protection. Restrict both
   environments to protected `v*` tags and add required maintainer reviewers.
4. Store the Apple credentials only as `release-signing` environment secrets;
   remove repository-level copies. `release-publication` needs no custom secret.
5. Make the repository public. A tag push while it is private fails before any
   build, signing, attestation, or publication job can run.

## macOS signing & notarization (one-time setup)

Tagged builds **fail** unless the protected `release-signing` environment and its
secrets exist. The signing job has no checkout, never compiles repository code,
and never executes its downloaded payload. Dry runs cannot enter this job.

Always required:

| Secret | Contents |
| --- | --- |
| `MACOS_CERT_P12` | base64 of the *Developer ID Application* certificate + private key (.p12) |
| `MACOS_CERT_PASSWORD` | the .p12 export password |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: <Your Org> (<TEAMID>)` |

Plus **one** of the two notary credential sets:

| Option A — Apple ID + app-specific password | Contents |
| --- | --- |
| `APPLE_ID` | the Apple ID email of a team member |
| `APPLE_TEAM_ID` | the 10-char team ID (e.g. `<TEAMID>`) |
| `APPLE_APP_PASSWORD` | an app-specific password ([account.apple.com](https://account.apple.com) → Sign-In and Security → App-Specific Passwords) |

| Option B — App Store Connect API key | Contents |
| --- | --- |
| `APPLE_API_KEY_ID` | API key ID (e.g. `<KEYID>`) |
| `APPLE_API_ISSUER` | issuer ID (UUID) |
| `APPLE_API_KEY_P8` | contents of the downloaded `AuthKey_<KEYID>.p8` |

The pipeline prefers the API key when both are configured. Trade-off: the
app-specific password is tied to a *person's* Apple ID (leaves the team or
rotates their password → releases break, and the password grants broad
account-API access), while an API key is team-scoped and least-privilege —
fine to start with A, consider migrating to B later.

```sh
# Option A setup:
gh secret set APPLE_ID -R FusionbaseHQ/agsh --env release-signing
gh secret set APPLE_TEAM_ID -R FusionbaseHQ/agsh --env release-signing --body "<TEAMID>"
gh secret set APPLE_APP_PASSWORD -R FusionbaseHQ/agsh --env release-signing
```

### 1. Export the Developer ID certificate as .p12

On a Mac that has the certificate (check with
`security find-identity -v -p codesigning`): open **Keychain Access → My
Certificates**, right-click *Developer ID Application: …* → **Export**, choose
`.p12`, set a strong export password. Then:

```sh
base64 -i DeveloperID.p12 | gh secret set MACOS_CERT_P12 \
  -R FusionbaseHQ/agsh --env release-signing
gh secret set MACOS_CERT_PASSWORD -R FusionbaseHQ/agsh --env release-signing
gh secret set MACOS_SIGN_IDENTITY -R FusionbaseHQ/agsh \
  --env release-signing \
  --body "Developer ID Application: <Your Org> (<TEAMID>)"
```

### 2. (Option B only) Create an App Store Connect API key

[appstoreconnect.apple.com → Users and Access → Integrations → App Store
Connect API](https://appstoreconnect.apple.com/access/integrations/api) →
**Team Keys** → generate a key with the **Developer** role. Note the **Key ID**
and **Issuer ID**, download the `.p8` (downloadable only once):

```sh
gh secret set APPLE_API_KEY_ID -R FusionbaseHQ/agsh --env release-signing
gh secret set APPLE_API_ISSUER -R FusionbaseHQ/agsh --env release-signing
gh secret set APPLE_API_KEY_P8 -R FusionbaseHQ/agsh --env release-signing \
  < AuthKey_XXXXXXXXXX.p8
```

### What the pipeline does with them

- downloads an exact unsigned artifact produced by the non-signing build job
  and rejects unexpected files before importing any secret,
- imports the certificate into a throwaway keychain (deleted before packaging),
- `codesign --options runtime --timestamp` (hardened runtime — required for
  notarization),
- signs the shell, launch helper, and optional preload library, submits all three
  with `xcrun notarytool submit --wait`, and fails unless the verdict is
  **Accepted**,
- packages the *signed* files into the release tarball (checksums therefore
  cover the signatures).

## Release integrity

Release assets are protected in three layers:

1. `checksums.txt` contains SHA-256 digests for every binary and source tarball
   and the installer.
2. Every release publication generates GitHub/Sigstore artifact attestations for
   every tarball, the installer, and `checksums.txt`.
3. The installer always checks SHA-256, and if `gh` is available it also runs
   `gh attestation verify` restricted to this repository's release workflow,
   exact `refs/tags/vX.Y.Z` source ref, and GitHub-hosted runners. Set
   `AGSH_REQUIRE_ATTESTATION=1` to make attestation verification mandatory.

GitHub artifact attestations only work for private repositories on Enterprise
Cloud plans. This project fails every release-tag push while the repository is
private rather than silently publishing without provenance. Manual dry runs do
not publish or attest anything.

The checksum manifest, installer, and tarballs share the same GitHub release
trust domain: the checksum alone detects corruption, but not a compromised
release account that replaces both files. Require the Sigstore-backed
attestation for independent provenance verification
(`AGSH_REQUIRE_ATTESTATION=1`). Repository rules must restrict creation of `v*`
tags and deny their update or deletion. The workflow revalidates the remote
annotated tag against the triggering commit immediately before attestation and
again before publication. It rejects tags that are not stable
`vMAJOR.MINOR.PATCH`, do not exactly match the workspace version, or lack exactly
one corresponding `## [X.Y.Z] - YYYY-MM-DD` changelog section.

Manual verification requires a GitHub CLI version that supports `--source-ref`:

```sh
version=vX.Y.Z
asset=agsh-$version-aarch64-apple-darwin.tar.gz
base="https://github.com/FusionbaseHQ/agsh/releases/download/$version"
curl --proto '=https' --tlsv1.2 -fsSLo install.sh \
  "$base/install.sh"
curl --proto '=https' --tlsv1.2 -fsSLo checksums.txt \
  "$base/checksums.txt"
curl --proto '=https' --tlsv1.2 -fsSLo "$asset" \
  "$base/$asset"
gh attestation verify install.sh \
  -R FusionbaseHQ/agsh \
  --signer-workflow FusionbaseHQ/agsh/.github/workflows/release.yml \
  --source-ref "refs/tags/$version" \
  --deny-self-hosted-runners

gh attestation verify checksums.txt \
  -R FusionbaseHQ/agsh \
  --signer-workflow FusionbaseHQ/agsh/.github/workflows/release.yml \
  --source-ref "refs/tags/$version" \
  --deny-self-hosted-runners

gh attestation verify "$asset" \
  -R FusionbaseHQ/agsh \
  --signer-workflow FusionbaseHQ/agsh/.github/workflows/release.yml \
  --source-ref "refs/tags/$version" \
  --deny-self-hosted-runners
grep -F "  install.sh" checksums.txt > selected-checksums.txt
grep -F "  $asset" checksums.txt >> selected-checksums.txt
test "$(wc -l < selected-checksums.txt | tr -d ' ')" = 2
shasum -a 256 -c selected-checksums.txt
```

### Notes on CLI-tool notarization

- **Stapling does not apply**: tickets can be stapled to `.app`/`.dmg`/`.pkg`,
  not to a bare Mach-O executable or dylib. Gatekeeper fetches each ticket online
  (keyed by that file's code-directory hash) the first time it assesses the file;
  those same signed files are placed inside the `.tar.gz`.
- **When it matters**: quarantine is only set by browsers and similar apps, so
  `curl | sh` installs never trip Gatekeeper. Notarization protects users who
  download tarballs from the Releases page in a browser, satisfies MDM/endpoint
  policies that require it, and is a prerequisite for a future Homebrew cask.
- Verify the shipped executables locally:
  `codesign --verify --strict -v ./agsh ./agsh-exec-helper` and assess each with
  `spctl -a -vv -t install` (expect `source=Notarized Developer ID`).

## Local pre-release signing/notarization smoke test

With the required repository setting enabled, published assets and their tags
are immutable. Never replace or re-sign an existing release; cut a new version
for any correction. The release build first applies temporary hardened ad-hoc
signatures to copies and verifies that deep-interception `DYLD_*` target bindings
survive the helper boundary without leaking private transport bindings; Developer
ID signing still operates on the original staged payload. Before packaging,
maintainers can exercise Apple's local signing/notarization path on all three
locally built Mach-O files:

```sh
codesign --force --sign "Developer ID Application: <Your Org> (<TEAMID>)" \
  --options runtime --timestamp libagsh_intercept.dylib
codesign --force --sign "Developer ID Application: <Your Org> (<TEAMID>)" \
  --options runtime --timestamp agsh-exec-helper
codesign --force --sign "Developer ID Application: <Your Org> (<TEAMID>)" \
  --options runtime --timestamp agsh
mkdir agsh-notarize
cp agsh agsh-exec-helper libagsh_intercept.dylib agsh-notarize/
ditto -c -k --keepParent agsh-notarize agsh-notarize.zip
# store credentials once — Apple ID + app-specific password…
xcrun notarytool store-credentials agsh-notary \
  --apple-id you@example.com --team-id <TEAMID> --password xxxx-xxxx-xxxx-xxxx
# …or API key: --key AuthKey_XXX.p8 --key-id KEYID --issuer ISSUER
xcrun notarytool submit agsh-notarize.zip --keychain-profile agsh-notary --wait
```

This smoke test does not produce release archives, checksums, or attestations;
the protected workflow remains the only publication path.
