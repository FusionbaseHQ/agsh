# Releasing agsh

Releases are built by [`.github/workflows/release.yml`](../.github/workflows/release.yml):
pushing a `v*` tag runs the full two-platform gate (fmt, clippy, cargo tests,
golden checks, bash/sh differential tests, and PTY interactive tests), builds
four targets (macOS arm64/x86_64, static-musl Linux x86_64/aarch64), **signs
and notarizes the macOS binaries**, and publishes a GitHub Release with
tarballs + `checksums.txt` and notes extracted from `CHANGELOG.md`.
`workflow_dispatch` dry-runs the whole matrix without publishing (and without
requiring signing secrets).

## Cutting a release

1. Update `CHANGELOG.md`: move `[Unreleased]` content under `## [X.Y.Z] - date`.
2. Bump `version` in the workspace `Cargo.toml` **and** the internal path-dep
   `version = "…"` requirements (grep for the old version across
   `crates/*/Cargo.toml`).
3. Commit, push, wait for CI green.
4. Optional: `gh workflow run release.yml --ref main` to dry-run the matrix.
5. `git tag -a vX.Y.Z -m "agsh vX.Y.Z" && git push origin vX.Y.Z` — done.

## macOS signing & notarization (one-time setup)

Tagged builds **fail** unless the signing secrets exist — an unsigned release
cannot slip out silently. Dry runs skip signing.

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
gh secret set APPLE_ID -R FusionbaseHQ/agsh            # you@example.com
gh secret set APPLE_TEAM_ID -R FusionbaseHQ/agsh --body "<TEAMID>"
gh secret set APPLE_APP_PASSWORD -R FusionbaseHQ/agsh  # xxxx-xxxx-xxxx-xxxx
```

### 1. Export the Developer ID certificate as .p12

On a Mac that has the certificate (check with
`security find-identity -v -p codesigning`): open **Keychain Access → My
Certificates**, right-click *Developer ID Application: …* → **Export**, choose
`.p12`, set a strong export password. Then:

```sh
base64 -i DeveloperID.p12 | gh secret set MACOS_CERT_P12 -R FusionbaseHQ/agsh
gh secret set MACOS_CERT_PASSWORD -R FusionbaseHQ/agsh    # paste the export password
gh secret set MACOS_SIGN_IDENTITY -R FusionbaseHQ/agsh \
  --body "Developer ID Application: <Your Org> (<TEAMID>)"
```

### 2. (Option B only) Create an App Store Connect API key

[appstoreconnect.apple.com → Users and Access → Integrations → App Store
Connect API](https://appstoreconnect.apple.com/access/integrations/api) →
**Team Keys** → generate a key with the **Developer** role. Note the **Key ID**
and **Issuer ID**, download the `.p8` (downloadable only once):

```sh
gh secret set APPLE_API_KEY_ID -R FusionbaseHQ/agsh       # the Key ID
gh secret set APPLE_API_ISSUER -R FusionbaseHQ/agsh       # the Issuer ID
gh secret set APPLE_API_KEY_P8 -R FusionbaseHQ/agsh < AuthKey_XXXXXXXXXX.p8
```

### What the pipeline does with them

- imports the certificate into a throwaway keychain (deleted afterwards),
- `codesign --options runtime --timestamp` (hardened runtime — required for
  notarization),
- zips the signed binary and submits it with `xcrun notarytool submit --wait`,
  failing the build unless the verdict is **Accepted**,
- packages the *signed* binary into the release tarball (checksums therefore
  cover the signature).

## Release integrity

Release assets are protected in three layers:

1. `checksums.txt` contains SHA-256 digests for every tarball.
2. Public release runs generate GitHub/Sigstore artifact attestations for every
   tarball and `checksums.txt`.
3. The installer always checks SHA-256, and if `gh` is available it also runs
   `gh attestation verify` for the downloaded tarball. Set
   `AGSH_REQUIRE_ATTESTATION=1` to make attestation verification mandatory.

GitHub artifact attestations only work for private repositories on Enterprise
Cloud plans. Private dry runs therefore skip the attestation step; make the repo
public before cutting the first public release if you want release attestations
on that tag.

Manual verification:

```sh
gh attestation verify agsh-vX.Y.Z-aarch64-apple-darwin.tar.gz -R FusionbaseHQ/agsh
shasum -a 256 -c checksums.txt
```

### Notes on CLI-tool notarization

- **Stapling does not apply**: tickets can be stapled to `.app`/`.dmg`/`.pkg`,
  not to bare Mach-O binaries. Gatekeeper fetches the ticket online (keyed by
  the binary's code-directory hash) the first time it assesses the binary —
  which also covers the copy inside our `.tar.gz`.
- **When it matters**: quarantine is only set by browsers and similar apps, so
  `curl | sh` installs never trip Gatekeeper. Notarization protects users who
  download tarballs from the Releases page in a browser, satisfies MDM/endpoint
  policies that require it, and is a prerequisite for a future Homebrew cask.
- Verify a shipped binary locally:
  `codesign --verify --strict -v ./agsh` and
  `spctl -a -vv -t install ./agsh` (expect `source=Notarized Developer ID`).

## Local one-off signing/notarization

For re-signing an already-published release by hand:

```sh
codesign --force --sign "Developer ID Application: <Your Org> (<TEAMID>)" \
  --options runtime --timestamp agsh
ditto -c -k agsh agsh.zip
# store credentials once — Apple ID + app-specific password…
xcrun notarytool store-credentials agsh-notary \
  --apple-id you@example.com --team-id <TEAMID> --password xxxx-xxxx-xxxx-xxxx
# …or API key: --key AuthKey_XXX.p8 --key-id KEYID --issuer ISSUER
xcrun notarytool submit agsh.zip --keychain-profile agsh-notary --wait
```
