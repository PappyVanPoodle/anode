<#
.SYNOPSIS
    Installs the Anode 1.3.0 SDK for the current user.

.DESCRIPTION
    Adds this SDK's bin\ folder (anode.exe, zn.exe, anode-lsp.exe) to the front
    of the user PATH and sets ANODE_HOME to the SDK folder, so projects can find
    the standard library in $env:ANODE_HOME\packages. Nothing is copied: the SDK
    runs from the folder it was extracted to. No administrator rights are needed.

.PARAMETER Uninstall
    Removes this SDK's bin\ folder from the user PATH and clears ANODE_HOME if it
    points here.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\install.ps1

.EXAMPLE
    .\install.ps1 -WhatIf      # show what would change, change nothing

.EXAMPLE
    .\install.ps1 -Uninstall
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$sdk = $PSScriptRoot
$bin = Join-Path $sdk 'bin'
foreach ($exe in 'anode.exe', 'zn.exe', 'anode-lsp.exe') {
    if (-not (Test-Path -LiteralPath (Join-Path $bin $exe))) {
        throw "bin\$exe is missing. Run install.ps1 from the extracted anode-1.3.0-windows-x64 folder."
    }
}

function Test-SameDir([string]$a, [string]$b) {
    return $a.Trim().TrimEnd('\') -ieq $b.Trim().TrimEnd('\')
}

# Read the raw user PATH so entries such as %USERPROFILE%\bin stay unexpanded.
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
try {
    $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $others = @($raw -split ';' | Where-Object { $_.Trim() -ne '' -and -not (Test-SameDir $_ $bin) })

    if ($Uninstall) {
        if ($PSCmdlet.ShouldProcess("user PATH and ANODE_HOME", "remove $bin")) {
            $key.SetValue('Path', ($others -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
            $current = [Environment]::GetEnvironmentVariable('ANODE_HOME', 'User')
            # Setting a user variable also broadcasts the environment change to new shells.
            if ($current -and (Test-SameDir $current $sdk)) {
                [Environment]::SetEnvironmentVariable('ANODE_HOME', $null, 'User')
            } else {
                [Environment]::SetEnvironmentVariable('ANODE_HOME', $current, 'User')
            }
            Write-Host "Removed $bin from the user PATH. Open a new terminal for the change to apply."
        }
        return
    }

    if ($PSCmdlet.ShouldProcess("user PATH and ANODE_HOME", "add $bin and set ANODE_HOME=$sdk")) {
        $key.SetValue('Path', ((@($bin) + $others) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
        [Environment]::SetEnvironmentVariable('ANODE_HOME', $sdk, 'User')
    }
} finally {
    $key.Close()
}

if ($WhatIfPreference) {
    return
}

# Make the SDK usable in this session too.
$sessionRest = @($env:Path -split ';' | Where-Object { $_.Trim() -ne '' -and -not (Test-SameDir $_ $bin) })
$env:Path = ((@($bin) + $sessionRest) -join ';')
$env:ANODE_HOME = $sdk

& (Join-Path $bin 'anode.exe') --version
Write-Host ""
Write-Host "Anode SDK: $sdk"
Write-Host "Standard library: $sdk\packages"
Write-Host "Next steps (new terminals pick up PATH automatically):"
Write-Host "  anode doctor            # checks for the MSVC Build Tools linker"
Write-Host "  anode new hello_anode   # starter project with AGENTS.md and CLAUDE.md"
Write-Host "  anode run hello_anode"
