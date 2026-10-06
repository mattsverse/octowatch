#!/bin/bash
# codesign needs the identity's keychain in the user search list even when
# --keychain explicitly names it. Preserve other keychains (including issuers).
previous_keychains=()
keychain_search_list_saved=false

use_signing_keychain() {
    local search_list
    search_list=$(security list-keychains -d user) || return
    while IFS= read -r path; do
        [[ -n "$path" ]] && previous_keychains+=("$path")
    done < <(printf '%s\n' "$search_list" | sed -E 's/^[[:space:]]*"(.*)"$/\1/')
    keychain_search_list_saved=true
    # The conditional array expansion also handles an empty list on Bash 3.2.
    security list-keychains -d user -s "$1" ${previous_keychains[@]+"${previous_keychains[@]}"}
}

restore_keychain_search_list() {
    if "$keychain_search_list_saved"; then
        security list-keychains -d user -s ${previous_keychains[@]+"${previous_keychains[@]}"}
    fi
}
