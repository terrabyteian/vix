#!/usr/bin/env bash
# Fixture: exercise bash highlighting.

set -euo pipefail

readonly MAX_RETRIES=3
readonly GREETING="hello, world"

greet() {
    local name="${1:-friend}"
    echo "hi, ${name}!"
}

double() {
    local x=$1
    echo $((x * 2))
}

main() {
    local -a names=("Ada" "Linus" "Grace")
    for i in $(seq 1 "$MAX_RETRIES"); do
        if (( i % 2 == 0 )); then
            echo "$GREETING -> $(greet "${names[0]}")"
        else
            echo "${GREETING^^}!!!"
        fi
    done

    if [[ -f /etc/hostname ]]; then
        echo "host: $(cat /etc/hostname)"
    fi

    local total=0
    for n in "${names[@]}"; do
        total=$((total + ${#n}))
    done
    echo "total chars: $total, doubled: $(double "$total")"
}

main "$@"
