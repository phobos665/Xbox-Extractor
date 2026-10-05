# Vendored: extract-xiso

Upstream: https://github.com/XboxDev/extract-xiso
Commit:   3f5b62cfe68f000b0e3c8a30104973f3a297948e

Copied unmodified except for dropping `.github/` and `.gitignore`. Built by
`app/scripts/build-sidecar.mjs` and shipped beside the app as a separate program
(a Tauri sidecar), which the Extract tab runs.

extract-xiso is under its own BSD-style licence (`LICENSE.TXT`), not this
project's GPL. It is never linked into the app; it runs as its own process.

This product includes software developed by in <in@fishtank.com>.
