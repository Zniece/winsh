trap 'echo done after loop' EXIT
for i in a b; do
    echo $i
done
