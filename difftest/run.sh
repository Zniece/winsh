#!/bin/bash
# Differential test: run each case through bash (WSL) and winsh (Windows) and
# compare stdout plus exit status.
#
# Unit tests verify "matches my understanding of POSIX". Only this harness
# verifies "matches bash" — that is a different claim, and the one scripts in
# the wild actually depend on.
#
# stderr is deliberately not compared: the two shells word their diagnostics
# differently, and matching bash's message text is not a goal.
#
# Layout: every *.sh file in cases/ is one case. The oracle is the system bash;
# winsh runs against the links-test coreutils farm so external commands
# behave identically on both sides.
#
# Run from anywhere:  bash difftest/run.sh
set -u
DIR=$(cd "$(dirname "$0")" && pwd)
WINSCRATCH=$(wslpath -w "$DIR")
WINSH="$WINSCRATCH\\..\\target\\debug\\winsh.exe"
TMP=$(mktemp)

pass=0; fail=0

for f in "$DIR"/cases/*.sh; do
    name=$(basename "$f")
    want=$(cd "$DIR" && timeout 10 bash "cases/$name" 2>/dev/null); wantc=$?
    ( cd "$DIR" && timeout 10 cmd.exe /c "set PATH=C:\\Users\\znie\\WSL\\bench\\bin\\links-test;C:\\Users\\znie\\.cargo\\bin;%PATH% && cd /d $WINSCRATCH && $WINSH cases\\$name" >"$TMP" 2>/dev/null )
    gotc=$?
    got=$(tr -d '\r' <"$TMP")
    if [ "$want" == "$got" ] && [ "$wantc" == "$gotc" ]; then
        pass=$((pass+1))
    else
        fail=$((fail+1))
        echo "FAIL $name"
        echo "  bash  (status $wantc):"
        printf '%s\n' "$want" | sed 's/^/          /' | head -12
        echo "  winsh (status $gotc):"
        printf '%s\n' "$got" | sed 's/^/          /' | head -12
    fi
done

rm -f "$TMP"
echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
