Set-Location $PSScriptRoot
if (-not (Test-Path .env)) { Copy-Item .env.example .env }
# CIVIL_HOST / CIVIL_PORT come from demo/.env (or the shell). 0.0.0.0 = phones on the same LAN; no auth.
python serve.py
