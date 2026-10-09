$ErrorActionPreference='Stop'
$projectRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location $projectRoot
try{
    & cargo build --release --locked
    if($LASTEXITCODE -ne 0){throw 'Release build failed.'}
    & (Join-Path $PSScriptRoot 'setup-pdfium.ps1') -Profile release
    $output=[IO.Path]::GetFullPath((Join-Path $projectRoot 'dist\document-organizer'))
    if(!$output.StartsWith($projectRoot+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)){throw 'Package output must remain inside this project.'}
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    foreach($name in @('document-organizer.exe','pdfium.dll')){Copy-Item -LiteralPath (Join-Path $projectRoot "target\release\$name") -Destination (Join-Path $output $name) -Force}
    Copy-Item -LiteralPath (Join-Path $projectRoot 'target\release\pdfium-licenses') -Destination $output -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md') -Destination $output -Force
    Copy-Item -LiteralPath (Join-Path $projectRoot 'LICENSE') -Destination $output -Force
    Write-Output "Portable application: $output"
}finally{Pop-Location}
