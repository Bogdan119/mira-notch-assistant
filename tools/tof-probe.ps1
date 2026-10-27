<#
.SYNOPSIS
    Live distance from the laptop's ST VL53L1 time-of-flight sensor.

.DESCRIPTION
    Read-only. Lenovo's sensor hub publishes the ToF sensor to Windows as
    *custom* sensors (not as a standard proximity sensor), which any app can
    read through Windows.Devices.Sensors.Custom:

      00000226-766d-4333-8262-27e82dd158b1  ST VL53L1 HOD  (distance + presence)
      00009c40-766d-4333-8262-27e82dd158b1  ST VL53L1 raw  (distance + ranging stats)

    Move a hand toward the screen and away: the bar follows the distance.

    Uses only Windows' own WinRT APIs (runs under Smart App Control). Needs
    Windows PowerShell 5.1 (powershell.exe), not PowerShell 7.

.EXAMPLE
    powershell -NoProfile -File tools\tof-probe.ps1 -Seconds 30
#>
param(
    [int]$Seconds = 30,
    [int]$IntervalMs = 40
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$asTask = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
    $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
} | Select-Object -First 1
function Await($op, [Type]$type) {
    $t = $asTask.MakeGenericMethod($type).Invoke($null, @($op))
    $t.Wait(-1) | Out-Null
    $t.Result
}

[void][Windows.Devices.Enumeration.DeviceInformation, Windows.Devices.Enumeration, ContentType = WindowsRuntime]
$Custom = [Windows.Devices.Sensors.Custom.CustomSensor, Windows.Devices.Sensors, ContentType = WindowsRuntime]

# Custom sensor values live under this property-key GUID; the number after it
# is the field. Learned by watching the readings (see below).
$K = '{C458F8A7-4AE8-4777-9607-2E9BDD65110A}'
$sensors = @(
    @{ Name = 'HOD'; Guid = '00000226-766d-4333-8262-27e82dd158b1'; Distance = "$K 161"; Extra = @{ Presence = "$K 90"; Confidence = "$K 162" } }
    @{ Name = 'ToF'; Guid = '00009c40-766d-4333-8262-27e82dd158b1'; Distance = "$K 162"; Extra = @{ Status = "$K 167" } }
)

function Open-Sensor($guid) {
    $sel = $Custom::GetDeviceSelector([Guid]$guid)
    $dev = Await ([Windows.Devices.Enumeration.DeviceInformation]::FindAllAsync($sel)) ([Windows.Devices.Enumeration.DeviceInformationCollection])
    if ($dev.Count -eq 0) { return $null }
    $s = Await ($Custom::FromIdAsync($dev[0].Id)) $Custom
    $s
}

function Get-Field($reading, [string]$key) {
    foreach ($kv in $reading.Properties) { if ($kv.Key -eq $key) { return $kv.Value } }
    $null
}

Write-Host "`n=== Mira · ToF distance probe ===`n" -ForegroundColor Cyan
foreach ($s in $sensors) {
    $s.Sensor = Open-Sensor $s.Guid
    if ($s.Sensor) {
        Write-Host ("[{0}] open, fastest report every {1} ms" -f $s.Name, $s.Sensor.MinimumReportInterval) -ForegroundColor Green
    } else {
        Write-Host ("[{0}] not found" -f $s.Name) -ForegroundColor Yellow
    }
}
$live = $sensors | Where-Object { $_.Sensor }
if (-not $live) { Write-Host 'No ToF sensor readable.' -ForegroundColor Yellow; exit 1 }

Write-Host "`nMove your hand toward the screen and away (Ctrl+C to stop).`n" -ForegroundColor Cyan
$end = (Get-Date).AddSeconds($Seconds)
$last = @{}
$min = [int]::MaxValue; $max = 0
while ((Get-Date) -lt $end) {
    foreach ($s in $live) {
        $r = $s.Sensor.GetCurrentReading()
        if (-not $r) { continue }
        $mm = Get-Field $r $s.Distance
        $extra = ($s.Extra.GetEnumerator() | ForEach-Object { "$($_.Key)=$(Get-Field $r $_.Value)" }) -join ' '
        # The timestamp ticks on every poll; only print when a value moves.
        $key = "$mm $extra"
        if ($last[$s.Name] -eq $key) { continue }
        $last[$s.Name] = $key
        if ($null -ne $mm) {
            $mm = [int]$mm
            if ($mm -gt 0) { $min = [Math]::Min($min, $mm); $max = [Math]::Max($max, $mm) }
            # 0-1000 mm mapped onto 50 columns; closer = shorter bar.
            $bar = '#' * [Math]::Min(50, [int]($mm / 20))
            $color = if ($mm -lt 150) { 'Red' } elseif ($mm -lt 350) { 'Yellow' } else { 'Gray' }
            Write-Host ("{0:HH:mm:ss.fff} {1} {2,5} mm  {3,-50} {4}" -f (Get-Date), $s.Name, $mm, $bar, $extra) -ForegroundColor $color
        }
    }
    Start-Sleep -Milliseconds $IntervalMs
}
if ($max -gt 0) { Write-Host ("`nRange seen: {0}-{1} mm" -f $min, $max) -ForegroundColor Cyan }
