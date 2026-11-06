x=VAL
cat <<END
x is $x
arith $((1+2))
sub $(echo hi)
esc \$x \\ \a
END
