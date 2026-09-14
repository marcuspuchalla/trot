# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_trot_global_optspecs
    string join \n h/help V/version
end

function __fish_trot_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_trot_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_trot_using_subcommand
    set -l cmd (__fish_trot_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c trot -n "__fish_trot_needs_command" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_needs_command" -s V -l version -d 'Print version'
complete -c trot -n "__fish_trot_needs_command" -f -a "daemon" -d 'Run the tracking daemon (talks to the treadmill, serves the API)'
complete -c trot -n "__fish_trot_needs_command" -f -a "diagnose" -d 'Record a bounded BLE support bundle (no upload or workout database)'
complete -c trot -n "__fish_trot_needs_command" -f -a "today" -d 'Today\'s totals'
complete -c trot -n "__fish_trot_needs_command" -f -a "status" -d 'Whether the daemon is up and a treadmill is connected'
complete -c trot -n "__fish_trot_needs_command" -f -a "log" -d 'Recent sessions'
complete -c trot -n "__fish_trot_needs_command" -f -a "scan" -d 'Scan for nearby treadmills and pick one to pair (interactive)'
complete -c trot -n "__fish_trot_needs_command" -f -a "devices" -d 'List paired treadmills (the active one is marked with *)'
complete -c trot -n "__fish_trot_needs_command" -f -a "pair" -d 'Pair a treadmill from `trot scan` and make it the active one'
complete -c trot -n "__fish_trot_needs_command" -f -a "unpair" -d 'Forget the active treadmill'
complete -c trot -n "__fish_trot_needs_command" -f -a "completions" -d 'Print (or install) a shell completion script'
complete -c trot -n "__fish_trot_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c trot -n "__fish_trot_using_subcommand daemon" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand diagnose" -l duration -d 'Capture duration, including discovery (1–600 seconds)' -r
complete -c trot -n "__fish_trot_using_subcommand diagnose" -l output -d 'Destination ZIP; never overwritten. A .partial.json file is retained on failure' -r -F
complete -c trot -n "__fish_trot_using_subcommand diagnose" -l device -d 'Exact advertised name or platform ID (standalone only; duplicate names require picker)' -r
complete -c trot -n "__fish_trot_using_subcommand diagnose" -l display-unit -d 'Console display unit for a standalone capture' -r -f -a "km/h\t''
mph\t''"
complete -c trot -n "__fish_trot_using_subcommand diagnose" -l note -d 'Optional console readings/model/firmware to accompany the trace; do not include secrets' -r
complete -c trot -n "__fish_trot_using_subcommand diagnose" -l inventory -d 'Only scan/discover GATT, with no telemetry queries or subscriptions (standalone)'
complete -c trot -n "__fish_trot_using_subcommand diagnose" -l non-interactive -d 'Do not show a picker. Requires --device when no daemon is running'
complete -c trot -n "__fish_trot_using_subcommand diagnose" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand today" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand status" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand log" -l limit -d 'How many to show' -r
complete -c trot -n "__fish_trot_using_subcommand log" -l week -d 'Only sessions from the last 7 days'
complete -c trot -n "__fish_trot_using_subcommand log" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand scan" -l seconds -d 'How long to scan for, in seconds (1–15)' -r
complete -c trot -n "__fish_trot_using_subcommand scan" -l all -d 'Show every Bluetooth device, not just treadmills'
complete -c trot -n "__fish_trot_using_subcommand scan" -l list -d 'Just print the list; don\'t show the interactive picker'
complete -c trot -n "__fish_trot_using_subcommand scan" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand devices" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand pair" -l name -d 'A friendly name to remember it by' -r
complete -c trot -n "__fish_trot_using_subcommand pair" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand unpair" -s h -l help -d 'Print help'
complete -c trot -n "__fish_trot_using_subcommand completions" -l install -d 'Write it where the shell will find it, instead of printing to stdout'
complete -c trot -n "__fish_trot_using_subcommand completions" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "daemon" -d 'Run the tracking daemon (talks to the treadmill, serves the API)'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "diagnose" -d 'Record a bounded BLE support bundle (no upload or workout database)'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "today" -d 'Today\'s totals'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "status" -d 'Whether the daemon is up and a treadmill is connected'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "log" -d 'Recent sessions'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "scan" -d 'Scan for nearby treadmills and pick one to pair (interactive)'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "devices" -d 'List paired treadmills (the active one is marked with *)'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "pair" -d 'Pair a treadmill from `trot scan` and make it the active one'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "unpair" -d 'Forget the active treadmill'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "completions" -d 'Print (or install) a shell completion script'
complete -c trot -n "__fish_trot_using_subcommand help; and not __fish_seen_subcommand_from daemon diagnose today status log scan devices pair unpair completions help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
