#!/usr/bin/env bash
# Stands in for `gh` inside the review run, so the bot's `gh api:*`
# permission cannot write with the user's credentials. Lets through
# `gh pr view`, `gh pr diff`, GET requests, and GraphQL queries.
set -euo pipefail

deny() {
    echo "review runner: $1" >&2
    exit 1
}

[[ ${1:-} == pr && ${2:-} =~ ^(view|diff)$ ]] && exec "$REAL_GH" "$@"
[[ ${1:-} == api ]] || deny "only gh pr view, gh pr diff and read-only gh api run here"

args=("$@")
shift
method=GET
explicit_method=0
fields=0
endpoint=""
query=""
field_names_plain=1
while (($#)); do
    arg=$1
    value=""
    case $arg in
    -X | --method | -f | -F | --field | --raw-field | -H | --header | -q | --jq | -t | --template | -p | --preview | --hostname | --cache)
        (($# >= 2)) || deny "$arg needs a value"
        value=$2
        shift
        ;;
    --*=*) value=${arg#*=} ;;
    -X?* | -f?* | -F?* | -H?* | -q?* | -t?* | -p?*) value=${arg:2} ;;
    --paginate | --slurp | -i | --include | --silent | --verbose) ;;
    -*) deny "$arg is not allowed" ;;
    *)
        [[ -z $endpoint ]] || deny "more than one endpoint"
        endpoint=$arg
        ;;
    esac
    case $arg in
    -X* | --method | --method=*) method=${value^^} explicit_method=1 ;;
    -f* | -F* | --field | --field=* | --raw-field | --raw-field=*)
        fields=1
        [[ $value =~ ^[A-Za-z_][A-Za-z0-9_]*= ]] || field_names_plain=0
        [[ $value == query=* ]] && query=${value#query=}
        ;;
    --input*) deny "--input is not allowed" ;;
    esac
    shift
done

if [[ $endpoint == graphql ]]; then
    ((explicit_method)) && deny "graphql takes no method"
    ((field_names_plain)) || deny "graphql field names must be plain"
    [[ -n $query && $query != @* ]] || deny "graphql needs an inline query"
    [[ ${args[*],,} =~ (^|[^a-z_])mutation([^a-z_]|$) ]] && deny "graphql mutations are not allowed"
elif [[ $method != GET ]] || { ((fields)) && ((!explicit_method)); }; then
    deny "only GET requests are allowed"
fi
exec "$REAL_GH" "${args[@]}"
