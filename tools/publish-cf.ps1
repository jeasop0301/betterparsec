# Publishes the BetterParsec distribution site to Cloudflare Workers:
# repackages the portable zip, uploads it to R2, and deploys the
# landing page + password-gated download worker from cf/.
#
# Prereqs: `npx wrangler login` once; DOWNLOAD_PASSWORD secret set via
# `npx wrangler secret put DOWNLOAD_PASSWORD` (run inside cf/).
$ErrorActionPreference = "Stop"

$root = Split-Path $PSScriptRoot -Parent

# 1. Rebuild the portable zip (also re-drops it into static/ for the
#    self-served host download path).
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "package-portable.ps1")

$zip = Join-Path $root "server/betterparsec-portable.zip"
if (-not (Test-Path $zip)) {
    throw "portable zip missing: $zip"
}

Push-Location (Join-Path $root "cf")
try {
    # 2. Upload the zip to R2 (worker streams it after password check).
    npx wrangler r2 object put betterparsec-dist/betterparsec-portable.zip --file $zip --remote
    if ($LASTEXITCODE -ne 0) { throw "r2 object put failed" }

    # 3. Deploy the worker + static assets.
    npx wrangler deploy
    if ($LASTEXITCODE -ne 0) { throw "wrangler deploy failed" }
} finally {
    Pop-Location
}
