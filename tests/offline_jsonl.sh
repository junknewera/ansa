set -eu

coproc OMH { exec "$1" --offline-jsonl; }
pid=$OMH_PID
source_input=${OMH[1]}
exec {output}<&"${OMH[0]}" {input}>&"$source_input"
exec {source_input}>&-

IFS= read -r -u "$output" line
[[ $line == *'"type":"snapshot"'* && $line == *'"state":"idle"'* && $line == *'"session":"offline"'* ]]
[[ $line =~ \"incarnation\":\"([a-f0-9]{32})\" ]]
incarnation=${BASH_REMATCH[1]}
printf '{"schema_version":1,"session":"offline","turn":"cli_turn","incarnation":"%s","command_id":"cli_submit","type":"submit","text":"fixture"}\n' \
  "$incarnation" >&"$input"

accepted=0
tool_started=0
tool_finished=0
cursor=
while IFS= read -r -u "$output" line; do
  case $line in
    *'"type":"command_accepted"'*)
      [[ $line == *'"command_id":"cli_submit"'* && $line == *'"request_id":"request_1"'* ]]
      accepted=1 ;;
    *'"type":"tool_started"'*) tool_started=1 ;;
    *'"type":"tool_finished"'*) tool_finished=1 ;;
    *'"type":"done"'*)
      [[ $line =~ \"stream_sequence\":([0-9]+) ]]
      cursor=${BASH_REMATCH[1]}
      break ;;
  esac
done
[[ $accepted == 1 && $tool_started == 1 && $tool_finished == 1 && -n $cursor ]]
printf '{"schema_version":1,"session":"offline","turn":"cli_turn","incarnation":"%s","command_id":"cli_snapshot","type":"snapshot","after_sequence":%s}\n' \
  "$incarnation" "$cursor" >&"$input"
exec {input}>&-

recovered=0
while IFS= read -r -u "$output" line; do
  if [[ $line == *'"type":"snapshot"'* && $line == *'"command_id":"cli_snapshot"'* ]]; then
    [[ $line == *'"state":"completed"'* && $line == *'"text":"OMH_TOOL_OK:OMH_FIXED_VALUE_42"'* ]]
    recovered=1
  fi
done
[[ $recovered == 1 ]]
wait "$pid"
