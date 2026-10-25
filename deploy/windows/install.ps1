# fMesh on Windows: the network module (meshd as a SYSTEM startup task) and the desktop module
# (tray plus native list for the logged-in user). Run from an elevated PowerShell in the unpacked
# package directory. Agent Mesh is not available on Windows yet.
#
#   .\install.ps1 -Module network -Name laptop9                    # key + join request
#   .\install.ps1 -Module network -Name laptop9 -AdminPub admin.pub -Roster roster
#   .\install.ps1 -Module desktop [-Upgrade]
#   add -DryRun to print the plan only
#
# Binaries live in C:\Program Files\flytoMesh (meshd.exe with wintun.dll beside it, as the existing
# startup task expects); node state (key, admin.pub, roster) in C:\ProgramData\flytoMesh.
# A node that already has a roster is left alone: it updates itself through signed releases.
# An installed desktop module is changed only with -Upgrade, which replaces its binaries.
# Binaries are replaced by copying to a new name and renaming; a running exe can only be
# renamed, so the old one is moved aside first.

param(
    [Parameter(Mandatory = $true)][ValidateSet('network', 'desktop')][string]$Module,
    [string]$Name,
    [string]$AdminPub,
    [string]$Roster,
    [switch]$Upgrade,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$Package = Split-Path -Parent $MyInvocation.MyCommand.Path
$Data = 'C:\ProgramData\flytoMesh'
$Bin = 'C:\Program Files\flytoMesh'

function Step([string]$What, [scriptblock]$Do) {
    if ($DryRun) { Write-Output "plan: $What"; return }
    & $Do
    Write-Output "done: $What"
}

function Install-Binary([string]$File, [string]$Dir) {
    $from = Join-Path $Package $File
    if (-not (Test-Path $from)) { throw "$File is missing from this package ($Package)" }
    $to = Join-Path $Dir $File
    Step "install $to" {
        New-Item -ItemType Directory -Force -Path $Dir | Out-Null
        $new = "$to.new"
        Copy-Item -Force $from $new
        if (Test-Path $to) {
            # A running exe cannot be overwritten or deleted, only renamed.
            $stale = "$to.stale-$([DateTime]::UtcNow.Ticks)"
            Rename-Item $to (Split-Path -Leaf $stale)
        }
        Rename-Item $new (Split-Path -Leaf $to)
    }
}

function Assert-Admin {
    $id = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $id.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'run this from an elevated PowerShell'
    }
}

if (-not $DryRun) { Assert-Admin }

if ($Module -eq 'network') {
    if (-not $Name) { throw '-Name is required (the node name the admin will put in the roster)' }
    if ([bool]$AdminPub -ne [bool]$Roster) { throw '-AdminPub and -Roster go together' }
    if (Test-Path (Join-Path $Data 'roster')) {
        throw "this machine is already a mesh node ($Data\roster exists). Nodes update themselves through signed releases; replacing meshd by hand would bypass that"
    }
    $meshd = Join-Path $Bin 'meshd.exe'
    Install-Binary 'meshd.exe' $Bin
    Install-Binary 'wintun.dll' $Bin
    Install-Binary 'fmesh.exe' $Bin
    if (-not $DryRun) { New-Item -ItemType Directory -Force -Path $Data | Out-Null }
    $key = Join-Path $Data 'key'
    if (Test-Path $key) {
        Write-Output "kept existing node key $key"
    } else {
        Step "generate the node key $key (never leaves this machine)" {
            # meshd prints the key; write it without a BOM and lock it to SYSTEM and Administrators.
            $k = (& $meshd genkey).Trim()
            [IO.File]::WriteAllText($key, "$k`n", (New-Object Text.UTF8Encoding $false))
            icacls $key /inheritance:r /grant:r 'SYSTEM:F' 'Administrators:F' | Out-Null
        }
    }
    if (-not $Roster) {
        if ($DryRun) { Write-Output "plan: print the join request for node $Name"; exit 0 }
        $pub = (Get-Content -Raw $key | & $meshd pubkey).Trim()
        Write-Output 'Join request (send this to the mesh admin; it holds no secret):'
        Write-Output "  name $Name"
        Write-Output "  key  $pub"
        Write-Output "The admin adds 'node <id> $Name <mesh IP> $pub' to the roster and signs it."
        Write-Output "Then run: .\install.ps1 -Module network -Name $Name -AdminPub <admin.pub> -Roster <roster>"
        exit 0
    }
    Step 'verify the roster against the admin key' {
        $admin = (Get-Content -Raw $AdminPub).Trim()
        $out = Get-Content -Raw $Roster | & $meshd roster-check --admin-pub $admin
        if ($LASTEXITCODE -ne 0) { throw 'the roster does not verify' }
        Write-Output $out
    }
    foreach ($pair in @(@($AdminPub, 'admin.pub'), @($Roster, 'roster'))) {
        $dest = Join-Path $Data $pair[1]
        if (Test-Path $dest) { Write-Output "kept existing $dest"; continue }
        Step "copy $($pair[0]) -> $dest" { Copy-Item $pair[0] $dest }
    }
    if (Get-ScheduledTask -TaskName 'flytoMesh' -ErrorAction SilentlyContinue) {
        Write-Output 'kept existing scheduled task flytoMesh'
    } else {
        Step 'register the startup task flytoMesh (SYSTEM, no time limit)' {
            $action = New-ScheduledTaskAction -Execute $meshd -Argument 'supervise'
            $trigger = New-ScheduledTaskTrigger -AtStartup
            # The default limit stops a task after 72 hours.
            $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
            Register-ScheduledTask -TaskName 'flytoMesh' -Action $action -Trigger $trigger -Settings $settings -User 'SYSTEM' -RunLevel Highest | Out-Null
            Start-ScheduledTask -TaskName 'flytoMesh'
        }
    }
    Write-Output 'Then: set the new mesh adapter to a Private network, keep the clock in sync (w32time), and check with fmesh status.'
}

if ($Module -eq 'desktop') {
    $installed = Get-ScheduledTask -TaskName 'flytoMesh Tray' -ErrorAction SilentlyContinue
    if ($installed -and -not $Upgrade) {
        throw "the desktop module is already installed (task 'flytoMesh Tray'); use -Upgrade to replace its binaries"
    }
    Install-Binary 'meshtray.exe' $Bin
    Install-Binary 'fmesh-desktop.exe' $Bin
    if (Get-ScheduledTask -TaskName 'flytoMesh Tray' -ErrorAction SilentlyContinue) {
        Write-Output "kept existing scheduled task 'flytoMesh Tray'"
    } else {
        Step "register 'flytoMesh Tray' at logon for the current user" {
            $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
            $action = New-ScheduledTaskAction -Execute (Join-Path $Bin 'meshtray.exe')
            $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
            $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero)
            Register-ScheduledTask -TaskName 'flytoMesh Tray' -Action $action -Trigger $trigger -Settings $settings -User $user | Out-Null
        }
    }
    Write-Output 'Device labels: %APPDATA%\flytomesh\devices.json (UTF-8). Agent lists need directory_command there.'
}
