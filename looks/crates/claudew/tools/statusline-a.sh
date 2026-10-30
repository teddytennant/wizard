#!/bin/sh
# A statusLine command for the comparison scenes: reads the session JSON, prints two rows.
input=$(cat)
dir=$(printf '%s' "$input" | sed -n 's/.*"project_dir":"\([^"]*\)".*/\1/p')
vim=$(printf '%s' "$input" | sed -n 's/.*"vim":{"mode":"\([A-Z ]*\)".*/\1/p')
printf '\033[36m%s\033[0m on \033[32mmain\033[0m \033[2m+2 -1\033[0m\n' "$(basename "$dir")"
printf 'plain row %s\n' "${vim:-}"
