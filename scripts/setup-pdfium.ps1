param([ValidateSet('debug','release')][string]$Profile='debug')
$ErrorActionPreference='Stop'
$projectRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runtime=Join-Path $projectRoot 'runtimes\pdfium-7543'
$archive=Join-Path $runtime 'pdfium-win-x64.tgz'
$expected='0b08b606792a6cc593426efdefc6622611bce446d9e0270743846956ea1554ca'
New-Item -ItemType Directory -Path $runtime -Force | Out-Null
if(!(Test-Path -LiteralPath $archive)){
    $download=Join-Path $runtime 'pdfium-win-x64.tgz.download'
    Invoke-WebRequest -Uri 'https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7543/pdfium-win-x64.tgz' -OutFile $download
    if((Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected){throw 'PDFium checksum mismatch; download retained for inspection.'}
    Move-Item -LiteralPath $download -Destination $archive
}
if((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected){throw 'Cached PDFium checksum mismatch.'}
& tar.exe -xzf $archive -C $runtime
if($LASTEXITCODE -ne 0){throw 'PDFium extraction failed.'}
$output=[IO.Path]::GetFullPath((Join-Path $projectRoot "target\$Profile"))
if(!$output.StartsWith($projectRoot+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)){throw 'Output must remain inside this project.'}
New-Item -ItemType Directory -Path $output -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $runtime 'bin\pdfium.dll') -Destination (Join-Path $output 'pdfium.dll') -Force
$licenses=Join-Path $output 'pdfium-licenses'
New-Item -ItemType Directory -Path $licenses -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $runtime 'LICENSE') -Destination (Join-Path $licenses 'LICENSE') -Force
Copy-Item -LiteralPath (Join-Path $runtime 'VERSION') -Destination (Join-Path $licenses 'VERSION') -Force
Copy-Item -LiteralPath (Join-Path $runtime 'licenses') -Destination $licenses -Recurse -Force
Write-Output "PDFium 7543 installed in $output (SHA256 verified)."
