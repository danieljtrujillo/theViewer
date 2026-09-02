# Builds theViewer when the sources are newer than the binary, then launches it.
# Driven by "Open theViewer.vbs" at the repository root, which runs this without a console.
#
# A release build of this dependency tree takes minutes, and the launcher runs hidden, so a plain
# double-click used to sit there doing nothing visible. Everything below the prerequisite checks
# exists to put a real progress window in front of that wait: what is compiling right now, how many
# units are done out of how many, and how long it has been going.
#
# The numbers are cargo's own. CARGO_TERM_PROGRESS_WHEN=always makes it emit its progress line even
# when its output is redirected to a file, which is the only reason an accurate count is available
# at all; without it cargo suppresses progress for anything that is not a terminal.

$ErrorActionPreference = 'Stop'
# Loaded up front: the failure paths below report through a message box, and this script runs with
# no console to print to.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$log = Join-Path $PSScriptRoot 'build.log'
$exe = Join-Path $root 'target\release\viewer.exe'
$iconFile = Join-Path $root 'assets\icon.ico'

$PRODUCT = 'theViewer'
$VENDOR = 'GANTASMO'

function Log($msg) {
    "$(Get-Date -Format 'HH:mm:ss')  $msg" | Add-Content -Path $log -Encoding utf8
}

function Show-Failure($text) {
    [System.Windows.Forms.MessageBox]::Show($text, $PRODUCT, 'OK', 'Error') | Out-Null
}

"" | Set-Content -Path $log -Encoding utf8
Log "root $root"

# Cargo, installed by rustup under the user profile.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (-not (Test-Path (Join-Path $cargoBin 'cargo.exe'))) {
    Log 'cargo not found'
    Show-Failure "The Rust toolchain is missing.`r`n`r`nInstall it with:`r`n    winget install Rustlang.Rustup"
    exit 1
}
$env:Path = "$cargoBin;$env:Path"

# The windows-gnu target links through MinGW binutils, which rustup does not ship.
$mingw = Get-ChildItem -Path (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages') `
    -Filter 'dlltool.exe' -Recurse -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($mingw) {
    $env:Path = "$($mingw.Directory.FullName);$env:Path"
    Log "mingw $($mingw.Directory.FullName)"
}

# Rebuild only when something the binary is built from is newer than the binary. `assets` is in the
# list because the icon is compiled into the executable as a resource, so regenerating it has to
# count as a source change.
$watched = @(
    (Join-Path $root 'crates'),
    (Join-Path $root 'assets')
) | Where-Object { Test-Path $_ }

$newest = Get-ChildItem $watched -Recurse -Include *.rs, *.wgsl, *.toml, *.ico, *.png -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
$stale = $true
if ((Test-Path $exe) -and $newest) {
    $stale = $newest.LastWriteTime -gt (Get-Item $exe).LastWriteTime
}

function Read-Shared($path) {
    # cargo holds the file open, so the default share mode fails intermittently.
    try {
        $fs = [System.IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
        $sr = New-Object System.IO.StreamReader($fs)
        $text = $sr.ReadToEnd()
        $sr.Close()
        $fs.Close()
        return $text
    } catch {
        return ''
    }
}

function Format-Span($span) {
    if ($span.TotalHours -ge 1) {
        return '{0}:{1:D2}:{2:D2}' -f [int]$span.TotalHours, $span.Minutes, $span.Seconds
    }
    return '{0}:{1:D2}' -f [int]$span.TotalMinutes, $span.Seconds
}

function Invoke-Build {
    $cargoLog = Join-Path $PSScriptRoot 'build-cargo.log'
    $cargoOut = Join-Path $PSScriptRoot 'build-cargo.out'
    Remove-Item $cargoLog, $cargoOut -ErrorAction SilentlyContinue

    # cargo suppresses its progress line whenever its output is not a terminal. Asking for it
    # anyway is what makes an accurate unit count available to parse.
    $env:CARGO_TERM_PROGRESS_WHEN = 'always'
    $env:CARGO_TERM_PROGRESS_WIDTH = '100'
    $env:CARGO_TERM_COLOR = 'never'

    $started = Get-Date
    $proc = Start-Process -FilePath 'cargo' `
        -ArgumentList 'build', '--release', '-p', 'viewer-app' `
        -WorkingDirectory $root -NoNewWindow -PassThru `
        -RedirectStandardError $cargoLog -RedirectStandardOutput $cargoOut

    # Reading `.Handle` once, here, is what makes `.ExitCode` readable later. `Start-Process
    # -PassThru` hands back a Process whose handle it does not keep open, and without this the
    # exit code comes back empty after the process ends, whatever it actually was. A successful
    # build then reports itself as a failure.
    $null = $proc.Handle

    $form = New-Object System.Windows.Forms.Form
    $form.Text = $PRODUCT
    $form.ClientSize = New-Object System.Drawing.Size(520, 196)
    $form.FormBorderStyle = 'FixedDialog'
    $form.StartPosition = 'CenterScreen'
    $form.MaximizeBox = $false
    $form.MinimizeBox = $false
    $form.ControlBox = $false
    $form.BackColor = [System.Drawing.Color]::FromArgb(250, 250, 252)
    if (Test-Path $iconFile) {
        try { $form.Icon = New-Object System.Drawing.Icon($iconFile) } catch {}
    }

    $art = New-Object System.Windows.Forms.PictureBox
    $art.SetBounds(24, 22, 56, 56)
    $art.SizeMode = 'Zoom'
    if (Test-Path $iconFile) {
        try { $art.Image = (New-Object System.Drawing.Icon($iconFile, 64, 64)).ToBitmap() } catch {}
    }
    $form.Controls.Add($art)

    $title = New-Object System.Windows.Forms.Label
    $title.SetBounds(96, 24, 400, 28)
    $title.Font = New-Object System.Drawing.Font('Segoe UI', 15, [System.Drawing.FontStyle]::Regular)
    $title.Text = $PRODUCT
    $form.Controls.Add($title)

    $by = New-Object System.Windows.Forms.Label
    $by.SetBounds(98, 52, 400, 18)
    $by.Font = New-Object System.Drawing.Font('Segoe UI', 8)
    $by.ForeColor = [System.Drawing.Color]::FromArgb(120, 124, 132)
    $by.Text = "by $VENDOR"
    $form.Controls.Add($by)

    $status = New-Object System.Windows.Forms.Label
    $status.SetBounds(26, 96, 468, 20)
    $status.Font = New-Object System.Drawing.Font('Segoe UI', 9)
    $status.Text = 'Preparing the build...'
    $form.Controls.Add($status)

    $bar = New-Object System.Windows.Forms.ProgressBar
    $bar.SetBounds(26, 122, 468, 16)
    $bar.Style = 'Marquee'
    $bar.MarqueeAnimationSpeed = 30
    $form.Controls.Add($bar)

    $detail = New-Object System.Windows.Forms.Label
    $detail.SetBounds(26, 146, 380, 20)
    $detail.Font = New-Object System.Drawing.Font('Segoe UI', 8)
    $detail.ForeColor = [System.Drawing.Color]::FromArgb(120, 124, 132)
    $detail.Text = 'First run compiles the whole dependency tree and takes several minutes.'
    $form.Controls.Add($detail)

    # A progress window with no way out is worse than no window. Cancelling kills the compiler and
    # everything it spawned, which is why it goes through taskkill rather than Stop-Process: rustc
    # runs as a child and would otherwise keep going with nothing watching it.
    $script:cancelled = $false
    $script:linking = $false
    $cancel = New-Object System.Windows.Forms.Button
    $cancel.SetBounds(414, 142, 80, 26)
    $cancel.Text = 'Cancel'
    $cancel.FlatStyle = 'System'
    $cancel.Add_Click({
        $script:cancelled = $true
        $cancel.Enabled = $false
        $status.Text = 'Cancelling...'
        try { & taskkill.exe /T /F /PID $proc.Id 2>&1 | Out-Null } catch {}
    })
    $form.Controls.Add($cancel)

    $timer = New-Object System.Windows.Forms.Timer
    $timer.Interval = 250
    $timer.Add_Tick({
        $text = Read-Shared $cargoLog

        # cargo's own count, which is the only accurate source of a total. The tail after the colon
        # is the set of units in flight, which is how the final link is recognised.
        $progress = [regex]::Matches($text, 'Building\s*\[[^\]]*\]\s*(\d+)/(\d+):?\s*([^\r\n]*)')
        if ($progress.Count -gt 0) {
            $last = $progress[$progress.Count - 1]
            $done = [int]$last.Groups[1].Value
            $total = [int]$last.Groups[2].Value
            if ($total -gt 0) {
                if ($bar.Style -ne 'Continuous') {
                    $bar.Style = 'Continuous'
                    $bar.Maximum = 100
                }
                # Floored, not rounded. 464 of 465 rounds to 100%, and a bar that reads 100 while
                # the longest step of the build is still running is the thing this window exists
                # to avoid.
                $pct = [int][Math]::Floor(100.0 * $done / $total)
                $bar.Value = [Math]::Max(0, [Math]::Min(100, $pct))

                $elapsed = (Get-Date) - $started
                $line = "$done of $total crates   $pct%   $(Format-Span $elapsed) elapsed"
                if ($done -gt 3 -and $done -lt $total) {
                    $remaining = [TimeSpan]::FromSeconds($elapsed.TotalSeconds / $done * ($total - $done))
                    $line += "   about $(Format-Span $remaining) left"
                }
                $detail.Text = $line

                # The binary is the only unit still in flight, so cargo is linking. It never
                # reaches done == total while this is running, because the unit being linked is
                # the one that has not finished.
                $script:linking = ($last.Groups[3].Value -match 'viewer\(bin\)')
            }
        }

        # What is being built right now, which is what "installing" means here.
        $items = [regex]::Matches($text, '(?m)^\s*(Compiling|Downloading|Updating|Installing)\s+(\S+)')
        if ($items.Count -gt 0) {
            $item = $items[$items.Count - 1]
            $status.Text = "$($item.Groups[1].Value) $($item.Groups[2].Value)"
        }
        # The binary is the last unit in flight and cargo has not said Finished, so this is the
        # link: fat link-time optimization across the whole tree, one step, routinely the longest,
        # and cargo reports nothing at all while it runs.
        #
        # Normalized first, because cargo redraws with a bare carriage return and `(?m)^` anchors
        # only after a newline. `Finished` is never at the start of a line as the regex sees it.
        $lines = $text -replace "`r", "`n"
        if ($script:linking -and $lines -notmatch '(?m)^\s*Finished\b') {
            $status.Text = 'Linking and optimizing viewer.exe'
            $detail.Text = 'The final step. Whole-program optimization, and it takes a few minutes.'
        }
        if ($lines -match '(?m)^\s*Finished\b') {
            $status.Text = 'Finished. Opening theViewer...'
            if ($bar.Style -eq 'Continuous') { $bar.Value = 100 }
        }

        if ($proc.HasExited) {
            $timer.Stop()
            $form.Close()
        }
    })

    $timer.Start()
    $form.ShowDialog() | Out-Null
    $timer.Stop()
    $form.Dispose()

    $proc.WaitForExit()
    $cargoText = Read-Shared $cargoLog

    # Cargo redraws its progress line with a bare carriage return, so most of its output is one
    # enormous line and `Finished` never begins one. `(?m)^` anchors after a newline only, never
    # after a CR, so every line-anchored pattern below has to run on normalized text.
    $cargoLines = $cargoText -replace "`r", "`n"
    $said_finished = $cargoLines -match '(?m)^\s*Finished\b'
    $said_error = $cargoLines -match '(?m)^\s*error(\[|:|\s)'

    # Only `$null` means the exit code is missing. Testing `$code -eq ''` as well looks harmless
    # and is not: PowerShell coerces the empty string, `0 -eq ''` is $true, and the guard then
    # fires on exactly the successful builds it was meant to let through.
    $code = $proc.ExitCode
    if ($null -eq $code) {
        Log 'cargo did not report an exit code; falling back to its output'
        if ($said_finished -and -not $said_error) { $code = 0 } else { $code = 1 }
    }

    # Fold cargo's output into the log the failure message points at.
    if ($cargoText) { Add-Content -Path $log -Value $cargoText -Encoding utf8 }
    if ((Test-Path $cargoOut) -and (Get-Item $cargoOut).Length -gt 0) {
        Add-Content -Path $log -Value (Read-Shared $cargoOut) -Encoding utf8
    }
    Remove-Item $cargoLog, $cargoOut -ErrorAction SilentlyContinue

    if ($script:cancelled) {
        Log 'build cancelled'
        return $false
    }
    if ($code -ne 0) {
        Log "build failed with $code"

        # A full disk is the one failure that cannot explain itself: cargo's message never reaches
        # the log, because writing the log is what runs out of space. Checking the drive directly
        # is the only way to say anything useful, and on this tree a release build wants several
        # gigabytes of headroom on its own.
        $freeGb = 0
        try {
            $drive = (Get-Item $root).PSDrive
            $freeGb = [math]::Round($drive.Free / 1GB, 1)
        } catch {}
        if ($cargoText -match 'not enough space|os error 112' -or $freeGb -lt 3) {
            Show-Failure ("The build ran out of disk space.`r`n`r`n" +
                "Drive $((Get-Item $root).PSDrive.Name): has $freeGb GB free. " +
                "A release build of this tree needs several GB.`r`n`r`n" +
                "Freeing space and running this again is enough; nothing is lost. " +
                "Deleting the target folder recovers the most and costs only a rebuild.")
            return $false
        }

        $tail = (($cargoLines -split "`n" | Where-Object { $_ -match '^\s*error' } | Select-Object -First 4) -join "`r`n")
        $message = "The build failed.`r`n`r`nSee $log"
        if ($tail) { $message = "The build failed.`r`n`r`n$tail`r`n`r`nSee $log" }
        Show-Failure $message
        return $false
    }
    Log 'build finished'
    return $true
}

if ($stale) {
    Log 'building'
    if (-not (Invoke-Build)) { exit 1 }
}

Log 'launching'
$viewerArgs = $args | Where-Object { $_ }
if ($viewerArgs) {
    Start-Process -FilePath $exe -ArgumentList $viewerArgs
} else {
    Start-Process -FilePath $exe
}
