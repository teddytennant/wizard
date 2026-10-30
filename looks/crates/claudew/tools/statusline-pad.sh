#!/bin/sh
# A statusLine command with padding: a short row, a red word and a row longer than any screen, wide characters.
cat > /dev/null
printf 'first row\n'
printf '\033[31mred\033[0m %s\n' "$(printf '%0150d' 0 | tr 0 x)"
printf 'wide 世界世界 end\n'
