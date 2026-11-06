x=VAL
cat <<'END'
raw $x and $((1+2)) and $(echo hi)
\a stays
END
