# Tokenboard — Copilot Edition

A private, local desktop dashboard for **Windows + GitHub Copilot** that reads the
history Copilot leaves on disk and turns it into usage analytics: where your time and
requests go, which repos cost the most premium requests, and what habits to fix.

- **Platform:** Windows 10/11 only · single MSI / NSIS installer · WebView2
- **AI source:** GitHub Copilot only (VS Code chat sessions + Copilot CLI) — no API keys
- **Stack:** Tauri 2 · Rust backend · static HTML + Chart.js frontend (~10 MB binary)
- **Privacy:** everything is read locally and read-only; the only network call is the
  Copilot CLI subprocess you trigger via "Extract AI lessons".

## Download & install (Windows x64)

Grab the latest build from the [**Releases**](https://github.com/prsasattms/Tokenboard/releases/latest) page:

| File | Type |
| --- | --- |
| `Tokenboard_1.0.0_x64_en-US.msi` | MSI installer |
| `Tokenboard_1.0.0_x64-setup.exe` | NSIS setup |

### "Windows protected your PC" / unknown publisher

These installers are **not code-signed**, so Windows SmartScreen and Defender flag them as
coming from an unknown publisher. This is a trust/reputation warning — **not** a virus
detection. (Once the repo is configured for signing — see **CI/CD → Code signing** —
signed release builds install without this prompt.) To proceed:

- On the SmartScreen dialog: click **More info → Run anyway**.
- Or unblock the file first in PowerShell:

  ```powershell
  Unblock-File .\Tokenboard_1.0.0_x64-setup.exe
  ```

### Verify your download (recommended)

Every release ships a `SHA256SUMS.txt`. Check that your file's hash matches:

```powershell
Get-FileHash -Algorithm SHA256 .\Tokenboard_1.0.0_x64-setup.exe
# compare the printed hash against the matching line in SHA256SUMS.txt
```

## What it shows (5 tabs)

| Tab | Content |
| --- | --- |
| **Overview** | 8 KPI cards · rule-based insight cards · "✦ Extract AI lessons" |
| **Tokens / Usage** | daily premium-request/token chart · usage doughnut · efficiency cards |
| **Repos & Sessions** | sortable per-repo and heaviest-session tables · error pills |
| **Tools & Models** | calls-vs-errors stacked bar · premium-requests-by-model doughnut |
| **Rhythm** | 24-cell hour-of-day heat strip · 7-bar weekday chart |

> **Premium requests, not dollars.** Copilot meters *premium requests* (per-model
> multipliers against a monthly allowance), not tokens or money. The `cost` figure is
> consumption, never a dollar amount. Token counts appear when Copilot exposes them;
> otherwise they read 0 and the view leads with request volume.

## Data sources (read-only)

- **Source A** — VS Code Copilot chat sessions
  `%APPDATA%\Code\User\workspaceStorage\<hash>\chatSessions\*.json`
  (`workspace.json` maps the opaque hash → real repo folder name).
  Also probes `Code - Insiders` and `globalStorage\github.copilot-chat`.
- **Source B** — Copilot CLI session store (additive; probed under
  `%LOCALAPPDATA%\github-copilot`, `%USERPROFILE%\.copilot`, etc.).

The Copilot on-disk schema is undocumented and shifts between VS Code releases, so all
Copilot-specific knowledge is isolated in `src-tauri/src/ingest/copilot.rs` behind a
**probe-first** workflow. Run `--probe` on the target machine and calibrate the adapter
against what you actually see.

## Project layout

```
app/
  package.json                  # devDeps: @tauri-apps/cli ^2
  src/
    index.html                  # dashboard (5 tabs)
    chart.umd.min.js            # vendored Chart.js, no CDN
  src-tauri/
    Cargo.toml  build.rs  tauri.conf.json
    icons/ { icon.ico, icon.png }
    src/
      main.rs                   # Tauri commands + --probe/--dump/--ai + CLI spawn
      core/
        model.rs                # Stats / Session / ModelAgg / DailyAgg / Event / CostModel
        analyzer.rs             # rollups + efficiency + build_insights + output JSON
      ingest/
        copilot.rs              # the adapter: Copilot files → Event stream
        probe.rs                # --probe schema dumper
```

## Prerequisites (Windows dev box)

```powershell
winget install Rustlang.Rustup
rustup default stable-x86_64-pc-windows-msvc
winget install OpenJS.NodeJS.LTS
winget install Microsoft.VisualStudio.2022.BuildTools   # "Desktop development with C++"
npm install -g @tauri-apps/cli@^2
```

## Build & run

```powershell
# live dev window
cd app
npm install
npx tauri dev

# calibrate the adapter against THIS machine's Copilot data (run first!)
cargo run --manifest-path src-tauri/Cargo.toml -- --probe   # dumps distinct keys/shapes

# headless checks (no window)
cargo run --manifest-path src-tauri/Cargo.toml -- --dump    # prints the full analyze() JSON
cargo run --manifest-path src-tauri/Cargo.toml -- --ai      # tests Copilot-CLI lesson extraction

# release installers (MSI + NSIS) -> src-tauri\target\release\bundle\
npx tauri build
```

### First-run notes

- **SmartScreen:** without a code-signing certificate, Windows shows "Windows protected
  your PC." One-time bypass: *More info → Run anyway*.
- **AI lessons auth:** if the Copilot CLI reports "not logged in", run `copilot` once
  interactively (or `gh auth login`) to sign in, then retry.

## CI/CD

`.github/workflows/release.yml` runs a single `windows-latest` job. Push a `v*` tag to
trigger; `tauri-action` builds both installers into one **draft** release for review.

### Code signing (optional — Azure Trusted Signing)

Unsigned installers trip the SmartScreen "unknown publisher" warning. The workflow
**automatically signs** the app and both installers when Azure Trusted Signing is
configured; with no config it builds unsigned (unchanged). To enable it:

1. **Azure (one-time):**
   - Create a **Trusted Signing account** and a **Certificate Profile** (Public Trust)
     in a supported region, and complete identity validation.
   - Create an **App registration** (service principal) and grant it the
     **Trusted Signing Certificate Profile Signer** role on the account.
   - Note the endpoint (e.g. `https://eus.codesigning.azure.net/`), the account name,
     and the certificate-profile name.

2. **GitHub repo settings** (Settings → Secrets and variables → Actions):

   | Kind | Name | Value |
   | --- | --- | --- |
   | Variable | `AZURE_TS_ENDPOINT` | signing endpoint URL |
   | Variable | `AZURE_TS_ACCOUNT` | Trusted Signing account name |
   | Variable | `AZURE_TS_CERT_PROFILE` | certificate profile name |
   | Secret | `AZURE_TENANT_ID` | service-principal tenant id |
   | Secret | `AZURE_CLIENT_ID` | service-principal client id |
   | Secret | `AZURE_CLIENT_SECRET` | service-principal client secret |

   Or from the CLI:

   ```powershell
   gh variable set AZURE_TS_ENDPOINT     --repo prsasattms/Tokenboard --body "https://<region>.codesigning.azure.net/"
   gh variable set AZURE_TS_ACCOUNT      --repo prsasattms/Tokenboard --body "<account-name>"
   gh variable set AZURE_TS_CERT_PROFILE --repo prsasattms/Tokenboard --body "<profile-name>"
   gh secret   set AZURE_TENANT_ID       --repo prsasattms/Tokenboard
   gh secret   set AZURE_CLIENT_ID       --repo prsasattms/Tokenboard
   gh secret   set AZURE_CLIENT_SECRET   --repo prsasattms/Tokenboard
   ```

Once `AZURE_TS_ENDPOINT` is set, the next tagged build signs the app + MSI + NSIS
installer automatically and no longer trips SmartScreen.

### Internal signing (ESRP, for Microsoft-internal distribution)

For a Microsoft-internal build, signing goes through **ESRP** (not self-served Trusted
Signing). ESRP runs behind internal infrastructure, so it can't be called from the public
GitHub Actions workflow — use the Azure DevOps pipeline at **`azure-pipelines.yml`**
instead. It builds the MSI + NSIS installers and signs them with the `EsrpCodeSigning`
task. Connect this repo in an Azure DevOps project, complete ESRP onboarding, fill in the
`<PLACEHOLDER>` variables at the top of `azure-pipelines.yml`, then run it (tag-triggered).
