# Authenticode sign Windows release binaries.
# Requires: signtool in PATH, cert thumbprint in QUBOX_SIGN_THUMBPRINT.
param(
  [string]$Path = "target\release",
  [string]$Thumbprint = $env:QUBOX_SIGN_THUMBPRINT
)

if (-not $Thumbprint) {
  Write-Error "Set QUBOX_SIGN_THUMBPRINT to the Authenticode certificate thumbprint."
  exit 2
}

$signtool = Get-Command signtool -ErrorAction SilentlyContinue
if (-not $signtool) {
  Write-Error "signtool.exe not found (Windows SDK)."
  exit 2
}

Get-ChildItem -Path $Path -Include qubox-daemon.exe,qubox-host-agent.exe,qubox-client-cli.exe -Recurse -ErrorAction SilentlyContinue |
  ForEach-Object {
    & signtool sign /sha1 $Thumbprint /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 $_.FullName
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Host "signed $($_.FullName)"
  }
