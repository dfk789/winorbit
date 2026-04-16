param(
    [string]$Repo = "dfk789/winorbit"
)

$command = "winorbit"
$url = "https://github.com/$Repo"

if ($env:OS -like "Windows*") {
    $os = "windows"
} else {
    Write-Error "Unsupported operating system. Only Windows is currently supported."
    exit 1
}

if ($env:PROCESSOR_ARCHITECTURE -eq "x86") {
    $arch = "32"
} elseif ($env:PROCESSOR_ARCHITECTURE -eq "AMD64") {
    $arch = "64"
} elseif ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    $arch = "arm64"
} else {
    Write-Error "Unsupported architecture."
    exit 1
}

$target = "$os-$arch"

try {
    $tag = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" | Select-Object -Expand tag_name
} catch {
    Write-Error "Failed to query releases for $Repo. Check the repository name and make sure releases are published."
    exit 1
}

$dest = "C:\Users\$env:USERNAME\AppData\Local\Programs\$command"
$archive = "$url/releases/download/$tag/$command-$tag-$target.zip"
$outfile = "$dest\$command.exe"
$config = "$dest\$command.ini"
$license = "$dest\LICENSE"

Write-Host "Repository:  $url"
Write-Host "Command:     $command"
Write-Host "Tag:         $tag"
Write-Host "Target:      $target"
Write-Host "Archive:     $archive"
Write-Host "Destination: $dest"

$temp = New-TemporaryFile

try {
    Invoke-WebRequest -Uri $archive -OutFile $temp -UseBasicParsing -ErrorAction Stop | Out-Null
} catch {
    Write-Error "Download failed. Please check the release archive name and your internet connection."
    exit 1
}

Move-Item $temp "$temp.zip"
Expand-Archive "$temp.zip" -DestinationPath $temp

if (-not (Test-Path $dest)) {
    New-Item -ItemType Directory -Path $dest | Out-Null
}
if (Test-Path $outfile) {
    $retry = $true
    while ($retry) {
        try {
            Remove-Item -Force $outfile -ErrorAction Stop
            $retry = $false
        } catch {
            $id = (Get-Process | Where-Object { $_.Path -eq $outfile }).Id
            if ($id) {
                Write-Error "$command.exe is currently running. Please close it before continuing."
                Pause
            } else {
                Write-Error "Failed to remove old $command.exe. Please try again."
            }
        }
    }
}

Move-Item "$temp\$command.exe" $outfile

if (Test-Path "$temp\$command.ini") {
    if (-not (Test-Path $config)) {
        Move-Item "$temp\$command.ini" $config
        Write-Host "Installed default $command.ini."
    } else {
        Write-Host "Preserved existing $command.ini."
    }
}

if (Test-Path "$temp\LICENSE") {
    Move-Item -Force "$temp\LICENSE" $license
}

Remove-Item -Force "$temp.zip"
Remove-Item -Force -Recurse "$temp"

Write-Host ""
Write-Host "Installation successful!"

if ($Host.UI.RawUI.KeyAvailable) {
    $ans = Read-Host -Prompt "Run winorbit.exe? (y/n)"
    if ($ans -eq "y") {
        & $outfile
        Write-Host "Run winorbit.exe successful!"
        exit
    }
}
Write-Host "Please double-click '$outfile' to run it."
