# Releasing agsh

Releases are built by [`.github/workflows/release.yml`](../.github/workflows/release.yml):
pushing a `v*` tag runs a two-platform test gate, builds four targets
(macOS arm64/x86_64, static-musl Linux x86_64/aarch64), **signs and notarizes
the macOS binaries**, and publishes a GitHub Release with tarballs +
`checksums.txt` and notes extracted from `CHANGELOG.md`. `workflow_dispatch`
dry-runs the whole matrix without publishing (and without requiring signing
secrets).

## Cutting a release

1. Update `CHANGELOG.md`: move `[Unreleased]` content under `## [X.Y.Z] - date`.
2. Bump `version` in the workspace `Cargo.toml` **and** the internal path-dep
   `version = "…"` requirements (grep for the old version across
   `crates/*/Cargo.toml`).
3. Commit, push, wait for CI green.
4. Optional: `gh workflow run release.yml --ref main` to dry-run the matrix.
5. `git tag -a vX.Y.Z -m "agsh vX.Y.Z" && git push origin vX.Y.Z` — done.

## macOS signing & notarization (one-time setup)

Tagged builds **fail** unless these six repository secrets exist — an unsigned
release cannot slip out silently. Dry runs skip signing.

| Secret | Contents |
| --- | --- |
| `MACOS_CERT_P12` | base64 of the *Developer ID Application* certificate + private key (.p12) |
| `MACOS_CERT_PASSWORD` | the .p12 export password |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: <Your Org> (<TEAMID>)` |
| `APPLE_API_KEY_ID` | App Store Connect API key ID (e.g. `<KEYID>`) |
| `APPLE_API_ISSUER` | App Store Connect API issuer ID (UUID) |
| `APPLE_API_KEY_P8` | contents of the downloaded `AuthKey_<KEYID>.p8` |

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

### 2. Create an App Store Connect API key (for `notarytool`)

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
xcrun notarytool store-credentials agsh-notary \
  --key AuthKey_XXX.p8 --key-id KEYID --issuer ISSUER   # once
xcrun notarytool submit agsh.zip --keychain-profile agsh-notary --wait
```
