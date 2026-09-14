
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'trot' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'trot'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'trot' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('daemon', 'daemon', [CompletionResultType]::ParameterValue, 'Run the tracking daemon (talks to the treadmill, serves the API)')
            [CompletionResult]::new('diagnose', 'diagnose', [CompletionResultType]::ParameterValue, 'Record a bounded BLE support bundle (no upload or workout database)')
            [CompletionResult]::new('today', 'today', [CompletionResultType]::ParameterValue, 'Today''s totals')
            [CompletionResult]::new('status', 'status', [CompletionResultType]::ParameterValue, 'Whether the daemon is up and a treadmill is connected')
            [CompletionResult]::new('log', 'log', [CompletionResultType]::ParameterValue, 'Recent sessions')
            [CompletionResult]::new('scan', 'scan', [CompletionResultType]::ParameterValue, 'Scan for nearby treadmills and pick one to pair (interactive)')
            [CompletionResult]::new('devices', 'devices', [CompletionResultType]::ParameterValue, 'List paired treadmills (the active one is marked with *)')
            [CompletionResult]::new('pair', 'pair', [CompletionResultType]::ParameterValue, 'Pair a treadmill from `trot scan` and make it the active one')
            [CompletionResult]::new('unpair', 'unpair', [CompletionResultType]::ParameterValue, 'Forget the active treadmill')
            [CompletionResult]::new('completions', 'completions', [CompletionResultType]::ParameterValue, 'Print (or install) a shell completion script')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'trot;daemon' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;diagnose' {
            [CompletionResult]::new('--duration', '--duration', [CompletionResultType]::ParameterName, 'Capture duration, including discovery (1–600 seconds)')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Destination ZIP; never overwritten. A .partial.json file is retained on failure')
            [CompletionResult]::new('--device', '--device', [CompletionResultType]::ParameterName, 'Exact advertised name or platform ID (standalone only; duplicate names require picker)')
            [CompletionResult]::new('--display-unit', '--display-unit', [CompletionResultType]::ParameterName, 'Console display unit for a standalone capture')
            [CompletionResult]::new('--note', '--note', [CompletionResultType]::ParameterName, 'Optional console readings/model/firmware to accompany the trace; do not include secrets')
            [CompletionResult]::new('--inventory', '--inventory', [CompletionResultType]::ParameterName, 'Only scan/discover GATT, with no telemetry queries or subscriptions (standalone)')
            [CompletionResult]::new('--non-interactive', '--non-interactive', [CompletionResultType]::ParameterName, 'Do not show a picker. Requires --device when no daemon is running')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;today' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;status' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;log' {
            [CompletionResult]::new('--limit', '--limit', [CompletionResultType]::ParameterName, 'How many to show')
            [CompletionResult]::new('--week', '--week', [CompletionResultType]::ParameterName, 'Only sessions from the last 7 days')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;scan' {
            [CompletionResult]::new('--seconds', '--seconds', [CompletionResultType]::ParameterName, 'How long to scan for, in seconds (1–15)')
            [CompletionResult]::new('--all', '--all', [CompletionResultType]::ParameterName, 'Show every Bluetooth device, not just treadmills')
            [CompletionResult]::new('--list', '--list', [CompletionResultType]::ParameterName, 'Just print the list; don''t show the interactive picker')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;devices' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;pair' {
            [CompletionResult]::new('--name', '--name', [CompletionResultType]::ParameterName, 'A friendly name to remember it by')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;unpair' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'trot;completions' {
            [CompletionResult]::new('--install', '--install', [CompletionResultType]::ParameterName, 'Write it where the shell will find it, instead of printing to stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'trot;help' {
            [CompletionResult]::new('daemon', 'daemon', [CompletionResultType]::ParameterValue, 'Run the tracking daemon (talks to the treadmill, serves the API)')
            [CompletionResult]::new('diagnose', 'diagnose', [CompletionResultType]::ParameterValue, 'Record a bounded BLE support bundle (no upload or workout database)')
            [CompletionResult]::new('today', 'today', [CompletionResultType]::ParameterValue, 'Today''s totals')
            [CompletionResult]::new('status', 'status', [CompletionResultType]::ParameterValue, 'Whether the daemon is up and a treadmill is connected')
            [CompletionResult]::new('log', 'log', [CompletionResultType]::ParameterValue, 'Recent sessions')
            [CompletionResult]::new('scan', 'scan', [CompletionResultType]::ParameterValue, 'Scan for nearby treadmills and pick one to pair (interactive)')
            [CompletionResult]::new('devices', 'devices', [CompletionResultType]::ParameterValue, 'List paired treadmills (the active one is marked with *)')
            [CompletionResult]::new('pair', 'pair', [CompletionResultType]::ParameterValue, 'Pair a treadmill from `trot scan` and make it the active one')
            [CompletionResult]::new('unpair', 'unpair', [CompletionResultType]::ParameterValue, 'Forget the active treadmill')
            [CompletionResult]::new('completions', 'completions', [CompletionResultType]::ParameterValue, 'Print (or install) a shell completion script')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'trot;help;daemon' {
            break
        }
        'trot;help;diagnose' {
            break
        }
        'trot;help;today' {
            break
        }
        'trot;help;status' {
            break
        }
        'trot;help;log' {
            break
        }
        'trot;help;scan' {
            break
        }
        'trot;help;devices' {
            break
        }
        'trot;help;pair' {
            break
        }
        'trot;help;unpair' {
            break
        }
        'trot;help;completions' {
            break
        }
        'trot;help;help' {
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
