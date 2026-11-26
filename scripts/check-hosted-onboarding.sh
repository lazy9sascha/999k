#!/bin/sh
# Run the local first-connection checks with synthetic enrollment and mailbox fixtures.
set -eu

output=$(mktemp)
trap 'rm -f "$output"' EXIT

run_case() {
    binary=$1
    test_name=$2
    if ! cargo test --locked --bin "$binary" "$test_name" -- --exact --test-threads=1 >"$output" 2>&1; then
        cat "$output"
        return 1
    fi
    # Cargo also exits successfully when no test matches.
    if ! grep -Fx "test $test_name ... ok" "$output"; then
        cat "$output"
        return 1
    fi
}

if [ "${1:-}" = "--hermes-smoke" ] && [ "$#" -eq 2 ]; then
    # The output can include provider details. Report only the outcome.
    if ! hermes chat --provider "$2" --oneshot -q 'Reply only with P999K_HERMES_OK, without punctuation.' >"$output" 2>&1; then
        if grep -qi 'no active subscription or usable credits' "$output"; then
            printf '%s\n' 'Hermes provider rejected inference because the account has no subscription or usable credits.' >&2
        else
            printf '%s\n' 'Hermes provider did not complete the synthetic request.' >&2
        fi
        exit 1
    fi
    if ! grep -Eq '^[[:space:]]*P999K_HERMES_OK[[:space:]]*$' "$output"; then
        printf '%s\n' 'Hermes did not return the expected synthetic reply.' >&2
        exit 1
    fi
    printf '%s\n' 'Hermes returned the synthetic reply. Peer pairing and project replies are separate.'
    exit 0
fi
if [ "$#" -ne 0 ]; then
    printf '%s\n' 'Usage: check-hosted-onboarding.sh [--hermes-smoke <provider>]' >&2
    exit 2
fi

cd "$(dirname "$0")/.."
run_case 999k-registration hosted_cloudflare::tests::approved_reservation_attaches_peers_to_the_same_owned_tunnel
run_case 999k hosted_enrollment::tests::selectable_enrollment_reuses_saved_origin_and_rejects_extra_routes
run_case 999k lifecycle::fresh_tests::orca_bootstraps_after_address_only_reservation
run_case 999k orca_onboard::remote::tests::probe_requires_anonymous_denial_and_authenticated_selected_tools
orca plugin test plugins/p-999k

printf '%s\n' 'Local onboarding checks passed. Live provider, peer pairing, and project reply checks are separate.'
