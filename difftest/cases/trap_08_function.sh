f() {
    trap 'echo from-f' EXIT
}
f
echo main
