# Prints the release notes for one version: its section of CHANGELOG.md, the link definitions it uses, and the
# install footer. The release workflow publishes this; run it locally to preview:
#   pwsh tools/release-notes.ps1 0.4.2
param([Parameter(Mandatory)][string]$Version)

$Version = $Version.TrimStart('v')
$lines = Get-Content (Join-Path $PSScriptRoot '..\CHANGELOG.md')
$start = [array]::FindIndex($lines, [Predicate[string]] { param($l) $l.StartsWith("## [$Version]") })
if ($start -lt 0) { throw "CHANGELOG.md has no section for $Version" }
$body = @()
for ($i = $start + 1; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^## \[' -or $lines[$i] -match '^\[[^\]]+\]: ') { break }
    $body += $lines[$i]
}
$text = ($body -join "`n").Trim()
if (-not $text) { throw "The $Version section of CHANGELOG.md is empty" }

# reference links the section uses, e.g. ([#3]) -> [#3]: https://...
$refs = $lines | Where-Object { $_ -match '^\[([^\]]+)\]: ' -and $text.Contains("[$($Matches[1])]") }
$compare = $lines | Where-Object { $_.StartsWith("[$Version]: ") } | ForEach-Object { $_.Substring($Version.Length + 4) }

$out = @($text, '')
if ($compare) { $out += "**Full diff:** $compare", '' }
$out += '## Install / update', '',
    'Quit MacDock (Windows-logo menu > Quit MacDock), replace `macdock.exe` with the one below and start it again. Check the download with `macdock.exe.sha256`.'
if ($refs) { $out += '', ($refs -join "`n") }
$out -join "`n"
