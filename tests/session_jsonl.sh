set -eu
session_root=$(mktemp -d)
trap 'rm -rf "$session_root"' EXIT
previous_incarnation=
for round in 1 2; do
  coproc OMH { exec "$1" --offline-jsonl --session "$session_root/session"; }
  pid=$OMH_PID
  source_input=${OMH[1]}
  exec {output}<&"${OMH[0]}" {input}>&"$source_input"
  exec {source_input}>&-
  IFS= read -r -u "$output" line
  [[ $line == *'"type":"snapshot"'* ]]
  if [[ $round == 1 ]]; then
    [[ $line == *'"state":"idle"'* ]]
  else
    [[ $line == *'"state":"completed"'* && $line == *'"history_items":5'* ]]
  fi
  [[ $line =~ \"incarnation\":\"([a-f0-9]{32})\" ]]
  incarnation=${BASH_REMATCH[1]}
  [[ $incarnation != "$previous_incarnation" ]]
  previous_incarnation=$incarnation
  printf '{"schema_version":1,"session":"offline","turn":"turn","incarnation":"%s","command_id":"submit","type":"submit","text":"fixture"}\n' \
    "$incarnation" >&"$input"
  seen=0
  while IFS= read -r -u "$output" line; do
    if [[ $round == 1 && $line == *'"type":"done"'* ]]; then seen=1; break; fi
    if [[ $round == 2 && $line == *'"type":"command_replayed"'* && $line == *'"state":"completed"'* ]]; then seen=1; break; fi
  done
  [[ $seen == 1 ]]
  exec {input}>&-
  while IFS= read -r -u "$output" line; do :; done
  exec {output}<&-
  wait "$pid"
done
