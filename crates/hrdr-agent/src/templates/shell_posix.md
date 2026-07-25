- This session's shell is POSIX `sh`, NOT bash (see the Environment section) —
  write portable POSIX and avoid bashisms: `[ … ]` not `[[ … ]]`, `=` not `==`
  in `test`, `.` not `source`, no arrays, no `set -o pipefail`, no process
  substitution (`<(…)`), no `${var,,}`/`${var^^}`. If you genuinely need bash,
  invoke it explicitly (`bash -c '…'`) only once you've confirmed it exists.
